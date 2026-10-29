//! Parallel blob classification: pread-from-workers schedule + decode loop.
//!
//! Schedules a list of blob offsets across worker threads. Each worker
//! `pread`s a compressed blob, decompresses, and runs a caller-supplied
//! closure on the decoded `PrimitiveBlock`. The consumer thread merges
//! per-blob results.
//!
//! Four modes:
//!
//! - [`parallel_classify_phase`]: per-blob result `R` is sent to the
//!   consumer for each blob (worker holds only persistent scratch state
//!   `S`). Use for dense paths where per-worker accumulation would be
//!   unbounded at planet scale.
//!
//! - [`parallel_classify_phase_ordered`]: the same, with `merge` called in
//!   schedule order and dispatch held at most a fixed window ahead of the
//!   earliest unmerged blob, which bounds the reorder memory. Use where the
//!   merge's output must not depend on worker timing.
//!
//! - [`parallel_classify_accumulate`]: per-worker accumulator `S` is held
//!   for the whole scan and merged once at completion. Use for sparse
//!   paths where the accumulator has a known small bound.
//!
//! - [`parallel_scan_blobs_raw`]: same shape as `parallel_classify_phase`
//!   but the worker hands the caller `&[u8]` of decompressed blob bytes,
//!   skipping `PrimitiveBlock` construction. Use when the caller only
//!   needs a small wire-format subset (a couple of fields per element)
//!   and parsing the full block is wasted work.
//!
//! Schedule construction lives in [`build_classify_schedule`] and
//! [`build_classify_schedules_split`].

use crate::error::{Error, Result};
use crate::read::input::{InputFile, InputSource};

/// One blob of a pread schedule: where its frame starts, which blob of the
/// file it is, and how to reach its payload. Carries the same location the
/// header walker stamps on its own errors, so a pool that fails on this
/// blob names the file, the blob ordinal and the frame offset.
///
/// 24 bytes, the size of the `(seq, offset, size)` tuple it replaced: the
/// ordering key the tuple carried is the entry's position in its schedule,
/// which the pools pass to their merge instead of storing it. `data_size`
/// fits `u32` because the header parse refuses a datasize at or above
/// `MAX_BLOB_DATASIZE` (32 MiB), and the header length fits `u16` because
/// `check_header_len` refuses 64 KiB and above; both constructors convert
/// checked all the same. The byte this frees holds the blob's kind claim.
///
/// # Kind claims
///
/// An entry built with [`KindClaims::Refuse`] carries the kind its blob's
/// indexdata claims (never the kind the phase wants: an unindexed blob
/// replicated into every per-kind schedule stays claimless), and every
/// block-decoding pool refuses the blob, after decoding it, when it holds an
/// element of another kind ([`PrimitiveBlock::refuse_foreign_kind`]): the
/// phase that scheduled it on the claim would drop that element, and the
/// phase that needed it never sees the blob. One compare per blob. An entry
/// built with [`KindClaims::Unchecked`] carries no claim and nothing is
/// checked; every `Unchecked` at a call site is a phase that has not yet
/// adopted the refusal.
///
/// [`PrimitiveBlock::refuse_foreign_kind`]: crate::PrimitiveBlock
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScheduledBlob {
    frame_start: u64,
    blob: u64,
    data_size: u32,
    header_len: u16,
    /// `0` for no enforced claim, else `1 +` the claimed kind's ordinal.
    claim: u8,
}

const _: () = assert!(std::mem::size_of::<ScheduledBlob>() == 24);

/// Whether a schedule's entries carry their blobs' kind claims for the
/// decoding pools to enforce. See [`ScheduledBlob`], "Kind claims".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KindClaims {
    /// Carry each indexed blob's claimed kind; a decoded element of another
    /// kind is `ErrorKind::IndexdataMismatch` with a `KindContradiction`.
    Refuse,
    /// Carry no claim. The phase believes its schedule unchecked.
    Unchecked,
}

impl ScheduledBlob {
    /// The schedule entry for the blob a header walk returned, carrying its
    /// indexdata kind claim under [`KindClaims::Refuse`].
    pub(crate) fn from_meta(
        meta: &crate::read::header_walker::BlobHeaderMeta,
        claims: KindClaims,
    ) -> Result<Self> {
        Self::build(
            meta.frame_start,
            meta.blob_index,
            meta.data_offset,
            meta.data_size,
            meta.index.as_ref().map(|i| i.kind),
            claims,
        )
        .ok_or_else(|| {
            Error::internal(format!(
                "blob {} at offset {} has a header length or datasize the walker should have \
                 refused",
                meta.blob_index, meta.frame_start
            ))
        })
    }

    /// An entry for a blob known by its frame and payload position but not
    /// its ordinal (a schedule of extract `BlobDesc`s): its failures are
    /// located with the file and the frame offset only. `claim` is the kind
    /// its indexdata claims, kept under [`KindClaims::Refuse`].
    pub(crate) fn at_frame(
        frame_start: u64,
        data_offset: u64,
        data_size: usize,
        claim: Option<crate::ElementKind>,
        claims: KindClaims,
    ) -> Result<Self> {
        Self::build(
            frame_start,
            Self::UNKNOWN_ORDINAL,
            data_offset,
            data_size,
            claim,
            claims,
        )
        .ok_or_else(|| {
            Error::internal(format!(
                "blob at offset {frame_start} has a header length or datasize the walker \
                 should have refused"
            ))
        })
    }

    fn build(
        frame_start: u64,
        blob: u64,
        data_offset: u64,
        data_size: usize,
        claim: Option<crate::ElementKind>,
        claims: KindClaims,
    ) -> Option<Self> {
        let header_len = data_offset
            .checked_sub(frame_start.checked_add(4)?)
            .and_then(|len| u16::try_from(len).ok())?;
        let data_size = u32::try_from(data_size).ok()?;
        let claim = match (claims, claim) {
            (KindClaims::Refuse, Some(kind)) => 1 + kind.ordinal(),
            _ => 0,
        };
        Some(Self {
            frame_start,
            blob,
            data_size,
            header_len,
            claim,
        })
    }

    /// The kind claim this entry enforces, if any.
    #[inline]
    pub(crate) fn claim(&self) -> Option<crate::ElementKind> {
        self.claim
            .checked_sub(1)
            .and_then(crate::ElementKind::from_ordinal)
    }

    /// Refuse `block`, decoded from this entry's blob, when it contradicts
    /// the entry's enforced claim. Every block-decoding pool calls this
    /// before handing the block to its caller.
    #[inline]
    fn refuse_foreign_kind(&self, block: &crate::PrimitiveBlock) -> Result<()> {
        block.refuse_foreign_kind(self.claim())
    }

    /// The ordinal of an entry built without one.
    const UNKNOWN_ORDINAL: u64 = u64::MAX;

    /// Byte offset of the blob's `Blob` message (its payload).
    #[inline]
    pub(crate) fn data_offset(&self) -> u64 {
        self.frame_start + 4 + u64::from(self.header_len)
    }

    /// Length of the blob's payload.
    #[inline]
    pub(crate) fn data_size(&self) -> usize {
        self.data_size as usize
    }

    /// Byte offset of the blob's frame (its length prefix).
    #[inline]
    pub(crate) fn frame_start(&self) -> u64 {
        self.frame_start
    }

    /// 0-based ordinal of the blob in its file, when the schedule knew it.
    #[inline]
    pub(crate) fn blob_index(&self) -> Option<u64> {
        (self.blob != Self::UNKNOWN_ORDINAL).then_some(self.blob)
    }

    /// An entry whose payload is `data_size` bytes at `data_offset`, with a
    /// zero-length header (tests of the prefetch arithmetic).
    #[cfg(test)]
    fn at(data_offset: u64, data_size: u32) -> Self {
        Self {
            frame_start: data_offset - 4,
            blob: 0,
            data_size,
            header_len: 0,
            claim: 0,
        }
    }

    /// This blob's location in `path`, for an error about it.
    #[cold]
    pub(crate) fn location(
        &self,
        path: &std::sync::Arc<std::path::Path>,
    ) -> crate::error::Location {
        crate::error::Location::new(
            Some(path),
            self.blob_index(),
            Some(crate::ByteOffset(self.frame_start)),
        )
    }
}

use crate::tunables::{
    DECODE_WORKERS_FALLBACK, DECODE_WORKERS_MIN, DECODE_WORKERS_RESERVED,
    DECOMPRESS_EXPANSION_GUESS, POOL_DESCRIPTOR_CHANNEL_DEPTH, POOL_RESULT_CHANNEL_DEPTH,
    WILLNEED_COALESCE_GAP_BYTES, WILLNEED_WINDOW_BYTES,
};

/// Coalesce body ranges in a walker-derived schedule. Schedule entries retain
/// file order, so bodies separated by up to a BlobHeader-sized gap are joined.
/// A zero-sized body has no range to advise: passing length zero to
/// `posix_fadvise` would instead mean through EOF.
fn coalesce_willneed_ranges(schedule: &[ScheduledBlob]) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for entry in schedule {
        let (offset, size) = (entry.data_offset(), entry.data_size() as u64);
        if size == 0 {
            continue;
        }
        let Some(end) = offset.checked_add(size) else {
            continue;
        };
        if let Some((previous_offset, previous_size)) = ranges.last_mut()
            && previous_offset
                .checked_add(*previous_size)
                .and_then(|previous_end| previous_end.checked_add(WILLNEED_COALESCE_GAP_BYTES))
                .is_some_and(|end_with_gap| offset <= end_with_gap)
            && let Some(previous_end) = previous_offset.checked_add(*previous_size)
            && let Some(extension) = end.checked_sub(previous_end)
            && let Some(extended_size) = previous_size.checked_add(extension)
        {
            *previous_size = extended_size;
        } else {
            ranges.push((offset, size));
        }
    }
    ranges
}

/// Sliding WILLNEED state for a pread dispatcher. The dispatcher advises about
/// 256 MiB ahead as it hands entries to workers, rather than prefetching the
/// entire schedule before any reads begin.
pub(crate) struct WillneedPrefetch {
    ranges: Vec<(u64, u64)>,
    next_range: usize,
    next_offset: u64,
    /// The invocation a failed advice counts into.
    invocation: crate::host::Invocation,
}

impl WillneedPrefetch {
    /// Prepare unconditional sliding WILLNEED prefetching for a dispatch
    /// schedule of `invocation`.
    pub(crate) fn new(schedule: &[ScheduledBlob], invocation: &crate::host::Recorder) -> Self {
        Self {
            ranges: coalesce_willneed_ranges(schedule),
            next_range: 0,
            next_offset: 0,
            invocation: invocation.handle().clone(),
        }
    }

    /// Advise enough coalesced ranges to keep the window ahead of `entry`.
    pub(crate) fn before_dispatch(
        &mut self,
        shared_file: &std::sync::Arc<std::fs::File>,
        entry: &ScheduledBlob,
    ) {
        let Some(window_end) = entry
            .data_offset()
            .checked_add(entry.data_size() as u64)
            .and_then(|end| end.checked_add(WILLNEED_WINDOW_BYTES))
        else {
            return;
        };

        while let Some(&(range_offset, range_size)) = self.ranges.get(self.next_range) {
            let Some(range_end) = range_offset.checked_add(range_size) else {
                self.next_range += 1;
                self.next_offset = 0;
                continue;
            };
            let offset = if self.next_offset == 0 {
                range_offset
            } else {
                self.next_offset
            };
            if offset >= window_end {
                break;
            }
            let end = range_end.min(window_end);
            let Some(size) = end.checked_sub(offset) else {
                self.next_range += 1;
                self.next_offset = 0;
                continue;
            };
            if end == range_end {
                self.next_range += 1;
                self.next_offset = 0;
            } else {
                self.next_offset = end;
            }

            // `size` is never zero here (zero-sized bodies are not coalesced
            // and the window end lies past `offset`), so this is never the
            // through-EOF form.
            crate::fadvise::advise(
                shared_file,
                offset,
                size,
                crate::fadvise::Advice::WillNeed,
                &self.invocation,
            );
        }
    }
}

/// Resolve a caller-supplied `threads: Option<usize>` override into a
/// concrete decode-thread count. `None` (the default for every existing
/// caller) picks `host::sized_workers` over the host resources, the
/// convention established by the pipelined reader and matching the
/// comment in `src/read/pipeline.rs::run_pipeline`. `Some(0)` is
/// treated identically to `None` so CLI flags that map "0 = auto"
/// pass through cleanly. `Some(n)` forces exactly `n` threads.
fn resolve_thread_count(threads: Option<usize>, resources: &dyn crate::host::Resources) -> usize {
    match threads {
        Some(n) if n > 0 => n,
        _ => crate::host::sized_workers(
            resources,
            DECODE_WORKERS_RESERVED,
            DECODE_WORKERS_MIN,
            DECODE_WORKERS_FALLBACK,
        ),
    }
}

/// Classify-phase blob filter. Returns `(skip, tag_skip)`: `skip=true` means
/// the caller should skip the blob; `tag_skip=true` means the skip was driven
/// by the tag-index filter (the caller should bump its tag-skip counter).
///
/// Takes the already-parsed `BlobIndex` and raw `tagdata` bytes so callers
/// can feed it from either a `BlobHeader` (via `hdr.index()` / `hdr.tag_index()`
/// materialisation) or a `HeaderWalker` `BlobHeaderMeta` (which already
/// parses `index` up-front and holds raw `tagdata` bytes).
///
/// It applies the kind and tag-key rules only and ignores `filter.node_bbox`,
/// though `BlobFilter::uses_index` counts a bbox as index-backed. That is
/// harmless today: only extract builds a bbox filter, and it consults
/// `wants_index` itself; its callers here (`tags_filter`, `tags_count`) never
/// set one. A caller that passes a bbox filter must apply the bbox itself.
///
/// Lives here rather than in `tags_filter` because the tag index is a
/// schedule-building concern: `tags_count` reaches the same blobs through
/// [`build_classify_schedule_blob_filtered`] and must apply the identical rule,
/// and two copies of "when may I skip this blob" is exactly how a filter starts
/// dropping matches on one path and not the other.
pub(crate) fn blob_filter_check(
    index: Option<&crate::blob_meta::BlobIndex>,
    tagdata: Option<&[u8]>,
    filter: Option<&crate::blob_meta::BlobFilter>,
) -> (bool, bool) {
    let Some(filter) = filter else {
        return (false, false);
    };
    if let Some(idx) = index {
        let dominated = matches!(
            idx.kind,
            crate::ElementKind::Node if !filter.want_nodes
        ) || matches!(
            idx.kind,
            crate::ElementKind::Way if !filter.want_ways
        ) || matches!(
            idx.kind,
            crate::ElementKind::Relation if !filter.want_relations
        );
        if dominated {
            return (true, false);
        }
    }
    if filter.has_tag_filter()
        && let Some(bytes) = tagdata
        && let Some(tag_idx) = crate::blob_meta::TagIndex::deserialize(bytes)
        && !filter.wants_tag_index(&tag_idx)
    {
        return (true, true);
    }
    (false, false)
}

/// Build a classification schedule from a header-only scan, optionally
/// filtering by element type. Returns `(schedule, shared_file)` ready for
/// [`parallel_classify_phase`].
///
/// Only OsmData blobs are included. When `kind_filter` is `Some`, only blobs
/// whose indexdata matches the given element type (plus blobs without
/// indexdata) are included. A file whose data blobs carry indexdata on some
/// and not others is refused, as every command-input walk refuses it
/// (`Frames::refuse_partial_index`); a wholly unindexed one is scheduled.
///
/// Walks headers via [`HeaderWalker`](crate::read::header_walker::HeaderWalker)
/// so blob bodies are not dragged into the page cache during the scan. The
/// [`InputFile`] handed back for `pread`-based body reads is the walker's own
/// file handle (opened with `posix_fadvise(RANDOM)`) and path, reused to
/// avoid a second `open` at scan end. A pinned `input` is opened from its
/// pin, so the walk and the workers read the file the run pinned.
pub(crate) fn build_classify_schedule<'a>(
    input: impl Into<InputSource<'a>>,
    kind_filter: Option<crate::ElementKind>,
    claims: KindClaims,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<ScheduledBlob>, InputFile)> {
    let (schedule, file, _skipped) =
        build_classify_schedule_blob_filtered(input, kind_filter, None, claims, host)?;
    Ok((schedule, file))
}

/// [`build_classify_schedule`] under [`KindClaims::Refuse`], also returning
/// each entry's whole indexdata claim (kind, range, count), by schedule
/// position, `None` for an unindexed blob. For a phase that checks the range
/// and count of the blobs it decodes against their claims
/// ([`ClaimWalk`](crate::blob_meta::ClaimWalk)): [`ScheduledBlob`] carries
/// only the kind.
pub(crate) fn build_classify_schedule_with_claims<'a>(
    input: impl Into<InputSource<'a>>,
    kind_filter: Option<crate::ElementKind>,
    host: &crate::host::Entered<'_>,
) -> Result<(
    Vec<ScheduledBlob>,
    Vec<Option<crate::blob_meta::BlobIndex>>,
    InputFile,
)> {
    let mut indexes = Vec::new();
    let (schedule, file, _skipped) = schedule_walk(
        input,
        kind_filter,
        None,
        false,
        KindClaims::Refuse,
        Some(&mut indexes),
        host,
    )?;
    Ok((schedule, indexes, file))
}

/// Read the first frame of `input` and require it to be the OSMHeader.
fn consume_leading_header(
    walker: &mut crate::read::header_walker::HeaderWalker,
    input: &std::path::Path,
) -> Result<()> {
    let first = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input))?;
    if first.blob_type != crate::blob::BlobKind::OsmHeader {
        return Err(crate::error::new_missing_header(input));
    }
    Ok(())
}

/// [`build_classify_schedule`] plus an optional blob-level tag filter.
///
/// The third return value is how many blobs the tag index let us skip, for the
/// caller to report.
///
/// `blob_filter` must be derived from the *same* expressions the per-element
/// pass will apply, or blobs holding matches get dropped. `tags_filter`'s
/// `blob_filter_from_expressions` is the one producer; it deliberately clears
/// the whole key set when any expression's key matcher cannot be expressed as
/// key-set membership, because the filter is a union.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedule_blob_filtered<'a>(
    input: impl Into<InputSource<'a>>,
    kind_filter: Option<crate::ElementKind>,
    blob_filter: Option<&crate::blob_meta::BlobFilter>,
    claims: KindClaims,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<ScheduledBlob>, InputFile, u64)> {
    schedule_walk(input, kind_filter, blob_filter, false, claims, None, host)
}

/// [`build_classify_schedule_blob_filtered`] exempt from the partial-index
/// refusal. Only for `cat --clean`: `cat` is the remedy for a partially
/// indexed file, and its clean pass decodes every scheduled blob and keeps
/// elements by their actual kind. For the same reason its entries carry no
/// kind claim ([`KindClaims::Unchecked`]): the clean pass rewrites every
/// element under fresh indexdata, so a false claim decides nothing it writes.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn build_classify_schedule_blob_filtered_accepting_partial<'a>(
    input: impl Into<InputSource<'a>>,
    kind_filter: Option<crate::ElementKind>,
    blob_filter: Option<&crate::blob_meta::BlobFilter>,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<ScheduledBlob>, InputFile, u64)> {
    schedule_walk(
        input,
        kind_filter,
        blob_filter,
        true,
        KindClaims::Unchecked,
        None,
        host,
    )
}

/// The one schedule walk. `indexes`, when given, receives each scheduled
/// entry's indexdata claim at the entry's position.
fn schedule_walk<'a>(
    input: impl Into<InputSource<'a>>,
    kind_filter: Option<crate::ElementKind>,
    blob_filter: Option<&crate::blob_meta::BlobFilter>,
    accept_partial: bool,
    claims: KindClaims,
    mut indexes: Option<&mut Vec<Option<crate::blob_meta::BlobIndex>>>,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<ScheduledBlob>, InputFile, u64)> {
    let source = input.into();
    let input = source.path();
    host.invocation.marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(source, host)?;
    if accept_partial {
        walker = walker.accepting_partial_index();
    }
    // Consume the first blob, which must be the OSMHeader: an empty file or
    // one that opens with anything else is MissingHeader.
    // `HeaderWalker::next_header` enforces header position (an OSMHeader
    // anywhere after the first frame is a hard error) but does not require
    // the first frame to be one, so that is checked here; without it a file
    // that opens with an OSMData blob would have that blob dropped unread.
    // Subsequent non-OsmData blobs are filtered below.
    consume_leading_header(&mut walker, input)?;
    host.invocation.marker("SCHEDULE_SCANNER_OPEN_END");

    host.invocation.marker("SCHEDULE_SCAN_LOOP_START");
    let mut schedule: Vec<ScheduledBlob> = Vec::new();
    let mut blobs_skipped_by_tag: u64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        // A tag-key-only blob filter prunes through tagdata, so missing
        // indexdata only matters to a kind filter or an index-backed blob
        // filter (same predicate as the pipelined reader's note).
        if kind_filter.is_some()
            || blob_filter.is_some_and(crate::blob_meta::BlobFilter::uses_index)
        {
            walker.observe_filter_index(meta.index.as_ref());
        }
        if let Some(filter_kind) = kind_filter
            && let Some(idx) = &meta.index
            && idx.kind != filter_kind
        {
            continue;
        }
        let (skip, tag_skip) =
            blob_filter_check(meta.index.as_ref(), meta.tagdata.as_deref(), blob_filter);
        if skip {
            if tag_skip {
                blobs_skipped_by_tag += 1;
            }
            continue;
        }
        // No payload-extent check here: `next_header` already refused a
        // body running past EOF, located at this blob's frame (see
        // `reference/truncation-handling.md`, shape 4). Every walker-built
        // schedule inherits that, so no builder repeats it.
        schedule.push(ScheduledBlob::from_meta(&meta, claims)?);
        if let Some(indexes) = indexes.as_deref_mut() {
            indexes.push(meta.index);
        }
    }
    host.invocation.marker("SCHEDULE_SCAN_LOOP_END");

    host.invocation.marker("SCHEDULE_SCANNER_DROP_START");
    let shared_file = walker.input();
    drop(walker);
    host.invocation.marker("SCHEDULE_SCANNER_DROP_END");

    host.invocation.counter("schedule_blobs", schedule.len());
    host.invocation
        .counter("schedule_tagidx_skipped_blobs", blobs_skipped_by_tag);
    Ok((schedule, shared_file, blobs_skipped_by_tag))
}

/// Like [`build_classify_schedule`] but returns three per-kind schedules
/// from a single header pass. At planet / Europe scale the header walk is
/// itself ~15 s; callers that need all three kinds (currently `check_refs`)
/// would otherwise pay that cost three times.
///
/// Blobs lacking indexdata are included in all three schedules (matching
/// the per-kind behaviour of `build_classify_schedule(.., Some(kind))`,
/// which only skips blobs whose indexdata reports a mismatched kind). A
/// partially indexed file is refused, as there.
/// Positions are local to each schedule (each is a contiguous 0..n range
/// for `parallel_classify_phase`).
///
/// The fifth return value is the highest `max_id` indexdata reports per kind,
/// indexed `[node, way, relation]`, or 0 for a kind whose blobs carry no
/// indexdata. It is a *sizing hint only* - indexdata is not verified anywhere,
/// so a consumer that allocates from it must still cope with an ID that
/// exceeds it (see `verify_ids`, which reports such an ID rather than
/// panicking). It exists so allocations track the file instead of a hard-coded
/// ceiling that the passage of time invalidates.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::type_complexity)]
pub(crate) fn build_classify_schedules_split<'a>(
    input: impl Into<InputSource<'a>>,
    claims: KindClaims,
    host: &crate::host::Entered<'_>,
) -> Result<(
    Vec<ScheduledBlob>,
    Vec<ScheduledBlob>,
    Vec<ScheduledBlob>,
    InputFile,
    [i64; 3],
)> {
    let source = input.into();
    let input = source.path();
    host.invocation.marker("SCHEDULE_SCANNER_OPEN_START");
    let mut walker = crate::read::header_walker::HeaderWalker::open(source, host)?;
    consume_leading_header(&mut walker, input)?;
    host.invocation.marker("SCHEDULE_SCANNER_OPEN_END");

    host.invocation.marker("SCHEDULE_SCAN_LOOP_START");
    let mut nodes: Vec<ScheduledBlob> = Vec::new();
    let mut ways: Vec<ScheduledBlob> = Vec::new();
    let mut rels: Vec<ScheduledBlob> = Vec::new();
    let mut max_ids: [i64; 3] = [0; 3];
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let reported_max = meta.index.as_ref().map_or(0, |i| i.max_id);
        let entry = ScheduledBlob::from_meta(&meta, claims)?;
        match meta.index.as_ref().map(|i| i.kind) {
            Some(crate::ElementKind::Node) => {
                max_ids[0] = max_ids[0].max(reported_max);
                nodes.push(entry);
            }
            Some(crate::ElementKind::Way) => {
                max_ids[1] = max_ids[1].max(reported_max);
                ways.push(entry);
            }
            Some(crate::ElementKind::Relation) => {
                max_ids[2] = max_ids[2].max(reported_max);
                rels.push(entry);
            }
            None => {
                walker.observe_filter_index(None);
                // Unindexed: visible to every kind filter in the legacy path,
                // so replicate to all three schedules here.
                nodes.push(entry);
                ways.push(entry);
                rels.push(entry);
            }
        }
    }
    host.invocation.marker("SCHEDULE_SCAN_LOOP_END");

    host.invocation.marker("SCHEDULE_SCANNER_DROP_START");
    let shared_file = walker.input();
    drop(walker);
    host.invocation.marker("SCHEDULE_SCANNER_DROP_END");

    host.invocation.counter("schedule_node_blobs", nodes.len());
    host.invocation.counter("schedule_way_blobs", ways.len());
    host.invocation
        .counter("schedule_relation_blobs", rels.len());
    Ok((nodes, ways, rels, shared_file, max_ids))
}

/// Run a parallel classification phase: pread workers decompress and classify
/// blobs, sending compact results to a consumer that merges them into ID sets.
///
/// Workers pread each scheduled blob's compressed data, decompress, build a
/// `PrimitiveBlock`, run the `classify` closure, and send the result. The
/// consumer calls `merge(position, r)` for each result, forwarding the blob's
/// position in `schedule` so callers that care (e.g. `verify_ids`, which
/// needs cross-blob monotonicity) can index by it. Callers that don't care
/// ignore it. A caller that needs results in schedule order uses
/// [`parallel_classify_phase_ordered`], whose window bounds what waits for a
/// slow blob; reordering here by hand bounds nothing.
///
/// `classify` is fallible: a failure of the work on one blob - the pread, the
/// decompression, the decode or the callback itself - is located with the
/// input, the blob ordinal and its frame offset (cold, only on the error
/// path) and ends the phase. A callback that read another file locates its
/// own error; that location is kept.
///
/// **Note:** `merge` is called in arbitrary worker-completion order, not blob
/// file order. Callers that need file-order processing must buffer by position.
/// Per-blob streaming classify: workers send `R` per blob, keep `S` for scratch.
///
/// Use for dense/hot paths (node classify, way classify) where per-worker
/// accumulation would be unbounded at planet scale. Each per-blob `R` is
/// bounded by the contents of one input blob. `S` persists across blobs for
/// scratch reuse (DenseNodeColumns, decompress buffers, etc.).
///
/// For sparse paths that want per-worker accumulation, use
/// [`parallel_classify_accumulate`].
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_phase<S: Send, R: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> Result<R> + Send + Sync,
    mut merge: impl FnMut(usize, R),
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    parallel_classify_phase_abortable(
        input,
        schedule,
        threads,
        worker_init,
        classify,
        |seq, r| {
            merge(seq, r);
            std::ops::ControlFlow::Continue(())
        },
        host,
    )
}

crate::counters::counter_set! {
    /// Blocking time in this module's two unordered pools,
    /// [`run_unordered_per_blob`] (behind [`parallel_classify_phase`],
    /// [`parallel_classify_phase_abortable`] and [`parallel_scan_blobs_raw`])
    /// and [`parallel_classify_accumulate`], for `brokkr sidecar --stalls`.
    /// The ordered phase runs on `crate::ordered_pool`, which records into
    /// that engine's own set (`ordered_pool_*_wait_ns`) instead.
    ///
    /// Every field is a `*_wait_ns` stall counter recorded through the gated
    /// operations in `crate::counters`: an operation that does not block costs
    /// only its `try_*` attempt, and one that blocks adds its wait once. Sums
    /// across threads, so a total can exceed wall time.
    ///
    /// One set for every caller, not one per command or phase: the pools are
    /// the same mechanism whoever runs them, and the question these answer
    /// (does the shared-receiver fan-out, the result channel or the consumer
    /// stall) is a question about the mechanism. Per-caller names would need a
    /// name threaded through every call site and would split one stall
    /// category per phase. One set per invocation
    /// (`crate::counters::InvocationMetrics`). Phase attribution comes instead
    /// from when the set is emitted: the invocation's totals so far, at the
    /// end of every run (success or failure), so each emission lands inside
    /// the caller's own phase markers, where it has them, and the step from
    /// the previous emission is that run's share.
    /// Every counter only grows, so the last emission is the invocation's
    /// total and the maximum per name, which `--stalls` reads, is that total;
    /// the read pipeline's `PipelineMetrics` set is emitted the same way.
    pub(crate) struct UnorderedPoolMetrics {
        /// The dispatcher blocked on a full descriptor channel: every worker
        /// busy and the channel's queue of schedule positions full. Expected
        /// on any decode-bound phase (one thread parked, by design); it is
        /// the read pipeline's `pipeline_raw_send_wait_ns` analogue.
        dispatch_send_wait_ns => "classify_unordered_dispatch_send_wait_ns",
        /// A worker blocked acquiring the mutex over the shared descriptor
        /// receiver, queued behind a peer that holds it. The holder keeps the
        /// lock across its receive, so this is the fan-out's queueing. Read
        /// it against `classify_unordered_desc_recv_wait_ns`: with that near
        /// zero (positions always waiting) this is the mutex's own cost; with
        /// that large, the holder was starved of positions and this is mostly
        /// peers queued behind the starved holder.
        desc_lock_wait_ns => "classify_unordered_desc_lock_wait_ns",
        /// A worker holding the descriptor receiver blocked on an empty
        /// channel: the dispatcher (and its `WILLNEED` advice) behind the
        /// workers.
        desc_recv_wait_ns => "classify_unordered_desc_recv_wait_ns",
        /// A per-blob worker blocked sending its result on a full result
        /// channel: the consumer's merge behind the workers. The accumulate
        /// pool records none: its result channel holds one slot per worker
        /// and each worker sends at most once, so that send never blocks.
        result_send_wait_ns => "classify_unordered_result_send_wait_ns",
        /// The per-blob pool's consumer blocked on an empty result channel:
        /// the calling thread idle for want of a decoded blob to merge
        /// (workers, or the dispatcher feeding them, behind the merge). The
        /// accumulate pool records none: its consumer has nothing to merge
        /// until workers finish their whole share, so its wait is the scan
        /// itself rather than a stall.
        result_recv_wait_ns => "classify_unordered_result_recv_wait_ns",
    }
}

/// A worker's next schedule position from the shared descriptor receiver, or
/// `None` once the dispatcher has finished (or gone). The guard covers only
/// the receive; the blob's work, the caller's callbacks and any send run
/// after it is released. The mutex guards no application state that recovery
/// could expose half updated, so a poisoned guard lets the remaining workers
/// go on; a worker's panic still propagates when the scope joins.
fn next_position(
    rx: &std::sync::Mutex<std::sync::mpsc::Receiver<usize>>,
    metrics: &UnorderedPoolMetrics,
) -> Option<usize> {
    let guard = crate::counters::lock_recording_wait(rx, &metrics.desc_lock_wait_ns);
    crate::counters::recv_recording_wait(&guard, &metrics.desc_recv_wait_ns).ok()
}

/// The consumer's cancellation checkpoint for the pread pools: before each
/// wait for a result, after each arrival and after normal closure, with the
/// receive gated through `crate::counters::recv_recording_wait` (a wait on
/// an empty channel added to `wait_ns`). Returning the error from the
/// consumer is the pools' existing early exit: the result receiver drops,
/// the workers' sends fail, and the dispatcher's send fails once the last
/// worker has gone.
fn recv_checked_recording_wait<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    cancel: &crate::host::Recorder,
    wait_ns: &std::sync::atomic::AtomicU64,
) -> Result<Option<T>> {
    cancel.check()?;
    let received = crate::counters::recv_recording_wait(rx, wait_ns);
    cancel.check()?;
    Ok(received.ok())
}

/// [`parallel_classify_phase`] whose merge closure can stop the scan.
///
/// The merge runs on the calling thread and cannot return a `Result`, so
/// callers that write output from it capture the first write error in a local
/// and check it once the phase returns. Capturing alone does not *stop*
/// anything: the schedule still ran to completion, so a planet-scale run whose
/// very first write failed paid the entire decode before surfacing the error.
/// Returning `Break` drops the result receiver, which fails the workers' sends
/// and then the dispatcher's, unwinding the scan promptly.
///
/// Aborting is not cancellation: blobs already in flight still finish, and the
/// caller remains responsible for reporting why it stopped.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_phase_abortable<S: Send, R: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> Result<R> + Send + Sync,
    merge: impl FnMut(usize, R) -> std::ops::ControlFlow<()>,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    run_unordered_per_blob(
        input,
        schedule,
        threads,
        || DecodeLocal {
            pool: crate::blob::DecompressPool::new(&host.invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            state: worker_init(),
        },
        |local: &mut DecodeLocal<S>, position, raw: &[u8]| {
            let mut buf =
                crate::blob::pool_get(Some(&local.pool), raw.len() * DECOMPRESS_EXPANSION_GUESS);
            crate::blob::decompress_blob_raw(raw, &mut buf)?;
            let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                buf,
                &local.pool,
                &mut local.st_scratch,
                &mut local.gr_scratch,
            )?;
            schedule[position].refuse_foreign_kind(&block)?;
            classify(&block, &mut local.state)
        },
        merge,
        host,
        &host.invocation.metrics().unordered,
    )
}

/// One unordered per-blob worker's block-decode state: its decompress pool
/// and block-parse scratch, beside the caller's state.
struct DecodeLocal<S> {
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    state: S,
}

/// The unordered per-blob pool behind [`parallel_classify_phase_abortable`]
/// and [`parallel_scan_blobs_raw`]: one dispatcher thread feeds schedule
/// positions through a bounded channel to `threads` workers, each of which
/// preads the blob's payload, runs `work` on it with its own local state and
/// sends one located result per blob through a second bounded channel; the
/// calling thread merges results in completion order.
///
/// `local` runs on each worker's own thread. The dispatcher, not the
/// workers, advances the sliding `WILLNEED` advice, so the advice stays
/// coupled to dispatch through the descriptor channel's bound. A merge
/// returning `Break` drops the result receiver, which fails the workers'
/// sends and then the dispatcher's. The ordered pools moved to
/// `crate::ordered_pool`; this one keeps its dispatcher because that is
/// where the advice lives, and no dispatch cost has been shown to recover.
///
/// Its four blocking points (dispatcher send, descriptor lock and receive,
/// result send, consumer receive) record into `metrics`, which is emitted
/// once the run's threads have joined; production callers pass the
/// invocation's set, a test its own.
#[expect(
    clippy::too_many_arguments,
    reason = "a private driver: the wrappers' parameters plus the counter set a test substitutes"
)]
fn run_unordered_per_blob<L, R: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    local: impl Fn() -> L + Sync,
    work: impl Fn(&mut L, usize, &[u8]) -> Result<R> + Sync,
    mut merge: impl FnMut(usize, R) -> std::ops::ControlFlow<()>,
    host: &crate::host::Entered<'_>,
    metrics: &UnorderedPoolMetrics,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule, &host.invocation);
    host.invocation.check()?;
    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = resolve_thread_count(threads, &*host.resources);

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<usize>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel::<(usize, crate::error::Result<R>)>(
        POOL_RESULT_CHANNEL_DEPTH,
    );
    let shared_file = input.file();

    // Declared before the scope, so it emits after the scope has joined the
    // workers, on a return or an unwind alike.
    let _emit = EmitOnDrop(metrics, &host.invocation);
    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for (position, entry) in schedule.iter().enumerate() {
                willneed.before_dispatch(shared_file, entry);
                if crate::counters::send_recording_wait(
                    &desc_tx,
                    position,
                    &metrics.dispatch_send_wait_ns,
                )
                .is_err()
                {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let work_ref = &work;
            let local_ref = &local;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let mut state = local_ref();

                while let Some(position) = next_position(&rx, metrics) {
                    let entry = &schedule[position];

                    let r: crate::error::Result<R> = (|| {
                        read_buf.resize(entry.data_size(), 0);
                        file.read_exact_at(&mut read_buf, entry.data_offset())?;
                        work_ref(&mut state, position, &read_buf)
                    })()
                    .map_err(|e| input.locate_blob(entry, e));
                    if crate::counters::send_recording_wait(
                        &tx,
                        (position, r),
                        &metrics.result_send_wait_ns,
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(desc_rx);
        drop(result_tx);

        // Moved into this closure so every return drops it before the scope
        // joins the workers (a borrow would keep it alive past the join and
        // leave a worker parked in `send`).
        let result_rx = result_rx;
        while let Some((seq, result)) =
            recv_checked_recording_wait(&result_rx, &host.invocation, &metrics.result_recv_wait_ns)?
        {
            if merge(seq, result?).is_break() {
                // Dropping the receiver - it falls out of scope when this
                // closure returns, before the scope joins - fails the workers'
                // `tx.send`, breaking their loops, which in turn fails the
                // dispatcher's `desc_tx.send`.
                break;
            }
        }
        Ok(())
    })
}

/// Emits an unordered metrics set when dropped: after a run's scope has
/// joined its threads, whether the run returned or unwound.
struct EmitOnDrop<'a>(&'a UnorderedPoolMetrics, &'a crate::host::Recorder);

impl Drop for EmitOnDrop<'_> {
    fn drop(&mut self) {
        self.0.emit_all(self.1);
    }
}

/// One ordered-classify worker's thread-local state: its read buffer,
/// decompress pool and block-parse scratch, beside the caller's state.
struct ClassifyLocal<S> {
    read_buf: Vec<u8>,
    pool: std::sync::Arc<crate::blob::DecompressPool>,
    st_scratch: Vec<(u32, u32)>,
    gr_scratch: Vec<(u32, u32)>,
    state: S,
}

/// The calling thread's side of [`parallel_classify_phase_ordered`]: the
/// caller's merge, and the sliding `WILLNEED` advice, which follows the
/// window's edge (the furthest position a worker may claim next).
struct ClassifyConsumer<'a, M> {
    merge: M,
    willneed: WillneedPrefetch,
    schedule: &'a [ScheduledBlob],
    file: &'a std::sync::Arc<std::fs::File>,
}

impl<R, M: FnMut(usize, R) -> Result<()>> crate::ordered_pool::OrderedConsumer<R>
    for ClassifyConsumer<'_, M>
{
    fn consume(&mut self, position: usize, item: R) -> crate::Result<()> {
        (self.merge)(position, item)
    }

    fn on_advance(&mut self, edge: usize) {
        if let Some(entry) = edge.checked_sub(1).and_then(|last| self.schedule.get(last)) {
            self.willneed.before_dispatch(self.file, entry);
        }
    }
}

/// [`parallel_classify_phase`] whose `merge` runs in schedule order, with
/// memory bounded by `window`, on the crate's ordered pool
/// (`crate::ordered_pool`).
///
/// Results still complete in worker order, and ordering alone would not bound
/// them - one slow blob at the head lets workers race arbitrarily far ahead -
/// so a worker may only start schedule position `k` once `k < merged +
/// window`. At most `window` results are therefore alive between classify
/// and the end of their merge. Workers stall in two places: sending into the
/// pool's result channel when the merge is behind (the channel is shallower
/// than a wide window), and at the window when the head blob is a full
/// window behind the fastest one. `None` takes the pool's default window for
/// the thread count.
///
/// The workers persist (with their `S` state, scratch and decompress pools)
/// across the whole schedule, and merge overlaps classification; a batch
/// barrier would idle every worker on the slowest blob of each batch and
/// serialize the merge behind the workers, which matters on merge-heavy
/// phases. `worker_init` runs on each worker's own thread, so `S` need not be
/// `Send`.
///
/// `merge` receives each entry's position in `schedule`, in order, and may
/// fail: its error ends the run at once. Failures surface in schedule order
/// (the lowest failing position's classify error, or the merge's own at its
/// position), located as in [`parallel_classify_phase`]. The sliding
/// `WILLNEED` advice tracks the window's edge, advised before the positions
/// it covers can be claimed.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn parallel_classify_phase_ordered<S, R: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    window: Option<std::num::NonZeroUsize>,
    worker_init: impl Fn() -> S + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> Result<R> + Sync,
    merge: impl FnMut(usize, R) -> Result<()>,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let decode_threads = resolve_thread_count(threads, &*host.resources);
    let window = window.unwrap_or_else(|| crate::ordered_pool::default_window(decode_threads));
    let file = input.file();
    let mut workers = vec![(); decode_threads];
    crate::ordered_pool::run_ordered(
        schedule.len(),
        &mut workers,
        crate::ordered_pool::PoolConfig {
            window,
            cancel: &host.invocation,
            waits: crate::ordered_pool::Waits::default(),
        },
        |_| ClassifyLocal {
            read_buf: Vec::new(),
            pool: crate::blob::DecompressPool::new(&host.invocation),
            st_scratch: Vec::new(),
            gr_scratch: Vec::new(),
            state: worker_init(),
        },
        |(), local: &mut ClassifyLocal<S>, position| {
            let entry = &schedule[position];
            (|| {
                local.read_buf.resize(entry.data_size(), 0);
                file.read_exact_at(&mut local.read_buf, entry.data_offset())?;
                let mut buf = crate::blob::pool_get(
                    Some(&local.pool),
                    entry.data_size() * DECOMPRESS_EXPANSION_GUESS,
                );
                crate::blob::decompress_blob_raw(&local.read_buf, &mut buf)?;
                let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                    buf,
                    &local.pool,
                    &mut local.st_scratch,
                    &mut local.gr_scratch,
                )?;
                entry.refuse_foreign_kind(&block)?;
                classify(&block, &mut local.state)
            })()
            .map_err(|e| input.locate_blob(entry, e))
        },
        &mut ClassifyConsumer {
            merge,
            willneed: WillneedPrefetch::new(schedule, &host.invocation),
            schedule,
            file,
        },
    )
}

/// Per-blob streaming scan over decompressed blob bytes (no `PrimitiveBlock`
/// construction).
///
/// Same shape as [`parallel_classify_phase`] but the worker hands the
/// caller `&[u8]` of decompressed blob bytes instead of a parsed
/// `PrimitiveBlock`. Callers are expected to walk the wire format
/// themselves via the helpers in `crate::scan::way`, `crate::scan::node`,
/// `crate::scan::relation`, etc.
///
/// Use when the caller only needs a small subset of the blob's fields
/// and parsing the full block (StringTable, group_ranges, dense_nodes
/// columns) is wasted work. ALTW pass 0 (way-ref node-id collection)
/// is the canonical caller: it iterates only `Way.refs()` and never
/// touches strings or info, so going through `PrimitiveBlock` was
/// dropping per-blob StringTable parse + `(u32, u32)` scratch
/// allocations onto the hot path for nothing.
///
/// Builds no `PrimitiveBlock`, so it records no kind set and enforces no
/// entry's kind claim: a raw walker that wants the refusal checks the group
/// field tags it already reads (as `check --locations` does).
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_scan_blobs_raw<S: Send, R: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(usize, &[u8], &mut S) -> crate::error::Result<R> + Send + Sync,
    mut merge: impl FnMut(usize, R),
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    run_unordered_per_blob(
        input,
        schedule,
        threads,
        || (Vec::<u8>::new(), worker_init()),
        |local: &mut (Vec<u8>, S), position, raw: &[u8]| {
            let (decompress_buf, state) = local;
            crate::blob::decompress_blob_raw(raw, decompress_buf)?;
            classify(position, decompress_buf, state)
        },
        |seq, r| {
            merge(seq, r);
            std::ops::ControlFlow::Continue(())
        },
        host,
        &host.invocation.metrics().unordered,
    )
}

/// Raises an accumulate run's stop flag if its worker unwinds, so a panic
/// stops the peers as an error does; the scope still re-raises the panic
/// when it joins.
struct StopOnUnwind<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Per-worker accumulation classify: workers accumulate into `S` across
/// all blobs, send `S` once at completion.
///
/// # When to use
///
/// The per-worker `S` is held for the duration of the whole scan and only
/// merged at the end. The safe usage envelope is determined by the upper
/// bound on per-worker `S` memory at the largest scale you support,
/// multiplied by the number of decode threads.
///
/// Safe: relation classify (~68 MB per worker at planet) and relation
/// closure members (~13 MB per worker, e.g.
/// `tags_filter::collect_relation_member_closure`). These are sparse paths
/// where `S` is dominated by a small set of relation-local IDs or metadata.
///
/// Unsafe in practice: per-worker `IdSet` accumulation of node IDs during
/// way classify. A worker can touch node IDs across the full planet range,
/// so each worker's bitmap is bounded by ID space, not by its work. Geocode
/// pass 1.5 left this pool for that reason: it writes one shared,
/// pre-allocated `IdSet` with atomic sets from its own claim-counter pool
/// (`geocode_index::builder::pass1_5`). While it was here it shipped at a
/// 14.59 GB peak RSS (planet), a figure this comment carried from
/// `76347122` (2026-04-17) until `95eea09b` dropped it; its run and host are
/// not recorded. If you add another caller like this, measure first.
///
/// **A previous caller in this category was migrated to
/// [`parallel_classify_phase`] on 2026-04-28** (commit `17b116c`,
/// `tags_filter::collect_way_node_dependencies`). Per-worker IdSet was the
/// 24 GB anon RSS peak at planet `--invert-match w/highway=primary`
/// (UUID `9044c456`); after the migration to per-blob `Vec<i64>` of node
/// refs through the 32-slot result channel, the same phase peaks at
/// ~7 GB total (UUID `7e74981a`). Mechanism: per-worker bitmap × N
/// workers is bounded by ID space × N, not by element count, so a few
/// dozen workers each with a planet-wide bitmap saturates RSS. Per-blob
/// `Vec<i64>` size follows the refs in that input blob, and the bounded result
/// channel caps the queue depth.
///
/// Unsafe: per-worker `Vec<i64>` accumulation of node IDs during dense
/// node classify (would be O(billions of i64) per worker). Use
/// [`parallel_classify_phase`] instead - its per-blob merge is bounded
/// by the contents of one input blob.
///
/// # When you can't migrate to `parallel_classify_phase`
///
/// `parallel_classify_phase`'s `classify` closure is `Fn + Sync`
/// (immutable borrow of captures) and the `merge` closure is `FnMut`
/// (mutable borrow). They run concurrently - classify on workers,
/// merge on the consumer thread. If your classify needs `&X` while
/// merge needs `&mut X` for the same `X`, the borrow checker correctly
/// rejects the closure pair. This is exactly the case in
/// `tags_filter::collect_relation_member_closure`'s convergence loop:
/// classify reads `&included_relation_ids`, merge calls
/// `included_relation_ids.set_if_new(id)`. Stay on
/// `parallel_classify_accumulate` for those - the merge runs after
/// the parallel scope, so there's no read/write conflict, and
/// per-worker `Vec<i64>` member-list state is bounded by member counts
/// (relation closure: ~tens of MB / worker at planet) rather than by
/// ID space.
///
/// # Failure
///
/// The first worker to fail (an error, or a panic) stops the others at
/// their next blob, so an early failure does not cost the rest of the
/// schedule; blobs already in hand still finish. A stopped worker sends no
/// state (one that finished before the failure may already have been
/// merged), and the run returns the failing worker's error.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn parallel_classify_accumulate<S: Send>(
    input: &InputFile,
    schedule: &[ScheduledBlob],
    threads: Option<usize>,
    worker_init: impl Fn() -> S + Send + Sync,
    classify: impl Fn(&crate::PrimitiveBlock, &mut S) -> Result<()> + Send + Sync,
    mut merge: impl FnMut(S),
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;

    let mut willneed = WillneedPrefetch::new(schedule, &host.invocation);
    host.invocation.check()?;
    if schedule.is_empty() {
        return Ok(());
    }

    let decode_threads = resolve_thread_count(threads, &*host.resources);
    let cancel = &host.invocation;
    // Raised by the first worker to fail. The consumer drops its receiver on
    // the first error it sees, but that does not reach the other workers:
    // they only learn of it at their one send, after their whole share of
    // the schedule, and the dispatcher keeps feeding them until then.
    let stop = std::sync::atomic::AtomicBool::new(false);
    let stop = &stop;

    let (desc_tx, desc_rx) = std::sync::mpsc::sync_channel::<usize>(POOL_DESCRIPTOR_CHANNEL_DEPTH);
    let desc_rx = std::sync::Arc::new(std::sync::Mutex::new(desc_rx));
    // One slot per worker, and each worker sends at most once, so a send
    // never blocks: no stall counter on it, nor on the consumer's receive,
    // whose wait is the scan itself (see `UnorderedPoolMetrics`).
    let (result_tx, result_rx) =
        std::sync::mpsc::sync_channel::<crate::error::Result<S>>(decode_threads);
    let shared_file = input.file();
    let metrics = &host.invocation.metrics().unordered;

    // Emits after the scope has joined, so every thread's waits are in; on
    // a failure or an unwind too.
    let _emit = EmitOnDrop(metrics, &host.invocation);
    std::thread::scope(|scope| -> Result<()> {
        scope.spawn(move || {
            for (position, entry) in schedule.iter().enumerate() {
                willneed.before_dispatch(shared_file, entry);
                if crate::counters::send_recording_wait(
                    &desc_tx,
                    position,
                    &metrics.dispatch_send_wait_ns,
                )
                .is_err()
                {
                    break;
                }
            }
        });

        for _ in 0..decode_threads {
            let rx = std::sync::Arc::clone(&desc_rx);
            let tx = result_tx.clone();
            let file = std::sync::Arc::clone(shared_file);
            let classify_ref = &classify;
            let worker_init_ref = &worker_init;
            scope.spawn(move || {
                let mut read_buf: Vec<u8> = Vec::new();
                let worker_pool = crate::blob::DecompressPool::new(cancel);
                let mut st_scratch: Vec<(u32, u32)> = Vec::new();
                let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
                let unwind_stops_peers = StopOnUnwind(stop);
                let mut state = worker_init_ref();

                // The consumer only hears from a worker once, at its end, so
                // the workers check cancellation themselves: inside the
                // closure whose result is sent, so a cancelled worker still
                // reports, and the consumer's first `?` then drops the
                // receiver. There is no sequence to keep whole. `Ok(false)`
                // is a worker stopped by a peer's failure.
                let result: crate::error::Result<bool> = (|| {
                    loop {
                        cancel.check()?;
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(false);
                        }
                        let Some(position) = next_position(&rx, metrics) else {
                            return Ok(true);
                        };
                        let entry = &schedule[position];

                        (|| -> crate::error::Result<()> {
                            read_buf.resize(entry.data_size(), 0);
                            file.read_exact_at(&mut read_buf, entry.data_offset())?;
                            let mut buf = crate::blob::pool_get(
                                Some(&worker_pool),
                                entry.data_size() * DECOMPRESS_EXPANSION_GUESS,
                            );
                            crate::blob::decompress_blob_raw(&read_buf, &mut buf)?;
                            let block = crate::block::PrimitiveBlock::from_vec_pooled_with_scratch(
                                buf,
                                &worker_pool,
                                &mut st_scratch,
                                &mut gr_scratch,
                            )?;
                            entry.refuse_foreign_kind(&block)?;
                            classify_ref(&block, &mut state)
                        })()
                        .map_err(|e| input.locate_blob(entry, e))?;
                    }
                })();

                match result {
                    Ok(true) => {
                        tx.send(Ok(state)).ok();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // Raised before the send, so no peer outlives the
                        // error by more than the blob it has in hand.
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        tx.send(Err(e)).ok();
                    }
                }
                drop(unwind_stops_peers);
            });
        }
        drop(desc_rx);
        drop(result_tx);

        for result in result_rx {
            merge(result?);
        }
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ScheduledBlob, WillneedPrefetch, coalesce_willneed_ranges};

    const UNCHECKED: super::KindClaims = super::KindClaims::Unchecked;

    /// Write a PBF with `blobs` single-node OSMData blobs and return its path.
    fn write_many_blob_pbf(dir: &std::path::Path, blobs: usize) -> std::path::PathBuf {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let path = dir.join("many.osm.pbf");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::None,
        );
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        for i in 0..blobs {
            let mut block = BlockBuilder::new();
            block.add_node(i64::try_from(i).unwrap() + 1, 0, 0, [("k", "v")], None);
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
        }
        writer.flush().unwrap();
        path
    }

    /// A node blob, then a way blob whose indexdata claims nodes.
    fn write_way_blob_claiming_nodes(dir: &std::path::Path) -> std::path::PathBuf {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let path = dir.join("lying.osm.pbf");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::None,
        );
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        let mut nodes = BlockBuilder::new();
        nodes.add_node(1, 0, 0, [("k", "v")], None);
        writer
            .write_primitive_block(nodes.take().unwrap().unwrap())
            .unwrap();
        let mut ways = BlockBuilder::new();
        ways.add_way(5, [("k", "v")], &[1], None);
        let mut lying = ways.take_owned().unwrap().unwrap();
        lying.index.kind = crate::ElementKind::Node;
        writer.write_primitive_block_owned(lying).unwrap();
        writer.flush().unwrap();
        path
    }

    /// Every block-decoding pool refuses a blob whose decoded elements
    /// contradict the kind claim its entry carries under `Refuse`, located at
    /// the blob; under `Unchecked` the same pools decode it as before, and
    /// the claimed-node schedule both modes build holds the lying blob.
    #[test]
    fn the_decoding_pools_refuse_a_contradicted_kind_claim() {
        let dir = crate::path_guard::tests::scratch();
        let path = write_way_blob_claiming_nodes(dir.path());
        let host = crate::host::Host::entered_for_tests();
        let expected = Some(crate::KindContradiction {
            claimed: crate::ElementKind::Node,
            found: crate::ElementKind::Way,
        });

        for claims in [super::KindClaims::Refuse, UNCHECKED] {
            let (nodes, ways, _rels, input, _) =
                super::build_classify_schedules_split(&path, claims, &host).unwrap();
            assert_eq!((nodes.len(), ways.len()), (2, 0), "{claims:?}");
            let refuse = claims == super::KindClaims::Refuse;
            assert_eq!(
                nodes[1].claim(),
                refuse.then_some(crate::ElementKind::Node),
                "{claims:?}"
            );

            let check = |result: crate::Result<()>, pool: &str| {
                if refuse {
                    let err = result.unwrap_err();
                    assert_eq!(err.kind_contradiction(), expected, "{pool}: {err}");
                    assert_eq!(err.blob_index(), Some(2), "{pool}: {err}");
                } else {
                    result.unwrap();
                }
            };
            let classify = |_: &crate::PrimitiveBlock, _: &mut ()| Ok(());
            check(
                super::parallel_classify_phase(
                    &input,
                    &nodes,
                    Some(2),
                    || (),
                    classify,
                    |_, ()| {},
                    &host,
                ),
                "unordered",
            );
            check(
                super::parallel_classify_phase_ordered(
                    &input,
                    &nodes,
                    Some(2),
                    None,
                    || (),
                    classify,
                    |_, ()| Ok(()),
                    &host,
                ),
                "ordered",
            );
            check(
                super::parallel_classify_accumulate(
                    &input,
                    &nodes,
                    Some(2),
                    || (),
                    classify,
                    |()| {},
                    &host,
                ),
                "accumulate",
            );
        }
    }

    /// An unindexed blob carries no claim even under `Refuse`: it stands in
    /// every per-kind schedule, and holds every kind it likes.
    #[test]
    fn an_unindexed_entry_carries_no_claim() {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("unindexed.osm.pbf");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::None);
        writer
            .write_header(&HeaderBuilder::new().build().unwrap())
            .unwrap();
        let mut ways = BlockBuilder::new();
        ways.add_way(5, [("k", "v")], &[1], None);
        writer
            .write_primitive_block_no_indexdata(ways.take().unwrap().unwrap())
            .unwrap();
        writer.flush().unwrap();

        let host = crate::host::Host::entered_for_tests();
        let (nodes, ways, rels, input, _) =
            super::build_classify_schedules_split(&path, super::KindClaims::Refuse, &host).unwrap();
        for schedule in [&nodes, &ways, &rels] {
            assert_eq!(schedule.len(), 1);
            assert_eq!(schedule[0].claim(), None);
        }
        super::parallel_classify_phase(
            &input,
            &nodes,
            Some(1),
            || (),
            |_, ()| Ok(()),
            |_, ()| {},
            &host,
        )
        .unwrap();
    }

    /// A merge closure returning `Break` must stop the scan, not merely stop
    /// merging.
    ///
    /// `getparents` and `repack` capture the first write error in a local
    /// because the merge cannot return a `Result`. Capturing alone left the
    /// phase running the whole schedule: at planet scale a failed first write
    /// cost the entire decode before the error surfaced. This pins that
    /// `Break` actually unwinds the dispatcher and the workers.
    #[test]
    fn a_breaking_merge_stops_the_scan_rather_than_draining_it() {
        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 400;
        let path = write_many_blob_pbf(dir.path(), BLOBS);

        let (schedule, shared_file) = super::build_classify_schedule(
            &path,
            None,
            UNCHECKED,
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(
            schedule.len(),
            BLOBS,
            "the fixture must actually have many blobs, or 'stopped early' is \
             indistinguishable from 'finished'"
        );

        let classified = std::sync::atomic::AtomicUsize::new(0);
        let mut merged = 0_usize;
        super::parallel_classify_phase_abortable(
            &shared_file,
            &schedule,
            Some(2),
            || (),
            |_block, ()| {
                classified.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
            |_position, ()| {
                merged += 1;
                std::ops::ControlFlow::Break(())
            },
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();

        assert_eq!(merged, 1, "the merge broke on its first call");
        // The descriptor and result channels are bounded (16 and 32), so at
        // most a few dozen blobs can be in flight when the receiver drops.
        // Half the schedule is a wide margin that still fails decisively
        // against the old behaviour, which classified all 400.
        let classified = classified.into_inner();
        assert!(
            classified < BLOBS / 2,
            "breaking must unwind the scan; {classified} of {BLOBS} blobs were \
             decoded after the merge asked to stop"
        );

        // Paired counterpart: a merge that never breaks must still see every
        // blob, so the assertion above is about `Break` and not about the
        // phase having become lossy.
        let all = std::sync::atomic::AtomicUsize::new(0);
        super::parallel_classify_phase(
            &shared_file,
            &schedule,
            Some(2),
            || (),
            |_block, ()| Ok(()),
            |_position, ()| {
                all.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(all.into_inner(), BLOBS, "a non-breaking scan is exhaustive");
    }

    /// A failing accumulate worker stops its peers instead of leaving them to
    /// drain the schedule.
    ///
    /// The consumer drops its receiver at the first error, but accumulate
    /// workers send once, at their end, so they used to notice only after
    /// scanning their whole share: an error on the first blob still cost the
    /// rest of the schedule before the run returned. Same for a panic, which
    /// the scope only re-raises once every worker has finished.
    #[test]
    fn a_failing_accumulate_worker_stops_its_peers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 400;
        let path = write_many_blob_pbf(dir.path(), BLOBS);
        let (schedule, shared_file) = super::build_classify_schedule(
            &path,
            None,
            UNCHECKED,
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(schedule.len(), BLOBS);

        let run = |fail: &(dyn Fn(usize) + Sync)| {
            let classified = AtomicUsize::new(0);
            let merged = AtomicUsize::new(0);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::parallel_classify_accumulate(
                    &shared_file,
                    &schedule,
                    Some(2),
                    || (),
                    |_block, ()| {
                        let n = classified.fetch_add(1, Ordering::Relaxed);
                        fail(n);
                        if n == 0 {
                            return Err(crate::Error::invalid_data("first blob fails"));
                        }
                        Ok(())
                    },
                    |()| {
                        merged.fetch_add(1, Ordering::Relaxed);
                    },
                    &crate::host::Host::entered_for_tests(),
                )
            }));
            (outcome, classified.into_inner())
        };

        // An error on the first blob. The bound is wide (the descriptor
        // channel holds 16, and each peer finishes the blob in hand) and
        // still fails decisively against the old full drain.
        let (outcome, classified) = run(&|_| {});
        let err = outcome
            .expect("an error is returned, not raised")
            .unwrap_err();
        assert!(err.to_string().contains("first blob fails"), "{err}");
        assert!(
            classified < BLOBS / 2,
            "an early error must stop the peers; {classified} of {BLOBS} blobs \
             were classified"
        );

        // A panic on the first blob stops the peers the same way, and is
        // still re-raised.
        let (outcome, classified) = run(&|n| assert_ne!(n, 0, "first blob panics"));
        assert!(outcome.is_err(), "the panic must still propagate");
        assert!(
            classified < BLOBS / 2,
            "an early panic must stop the peers; {classified} of {BLOBS} blobs \
             were classified"
        );

        // Paired counterpart: a run that never fails is exhaustive.
        let all = AtomicUsize::new(0);
        super::parallel_classify_accumulate(
            &shared_file,
            &schedule,
            Some(2),
            || 0_usize,
            |_block, seen| {
                *seen += 1;
                Ok(())
            },
            |seen| {
                all.fetch_add(seen, Ordering::Relaxed);
            },
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(all.into_inner(), BLOBS, "a clean run classifies every blob");
    }

    /// The per-blob pool records its stalls into the set it is given, under
    /// the names `--stalls` reads.
    ///
    /// One worker, slower per blob than its dispatcher and its consumer, and
    /// more blobs than the descriptor channel holds: the dispatcher must find
    /// that channel full, and the consumer must find the result channel empty,
    /// so both record a wait. No duration is asserted, only that a wait was
    /// recorded where the pool blocked, and that a lone worker, which no peer
    /// can hold the descriptor lock against, records no lock wait at all.
    #[test]
    fn the_unordered_pool_records_where_it_blocks() {
        use std::sync::atomic::Ordering::Relaxed;

        let dir = crate::path_guard::tests::scratch();
        let blobs = super::POOL_DESCRIPTOR_CHANNEL_DEPTH + 8;
        let path = write_many_blob_pbf(dir.path(), blobs);
        let host = crate::host::Host::entered_for_tests();
        let (schedule, input) =
            super::build_classify_schedule(&path, None, UNCHECKED, &host).unwrap();
        assert_eq!(schedule.len(), blobs);

        let metrics = super::UnorderedPoolMetrics::new();
        let mut merged = 0_usize;
        super::run_unordered_per_blob(
            &input,
            &schedule,
            Some(1),
            || (),
            |_: &mut (), _: usize, _: &[u8]| -> crate::Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(2));
                Ok(())
            },
            |_, ()| {
                merged += 1;
                std::ops::ControlFlow::Continue(())
            },
            &host,
            &metrics,
        )
        .unwrap();
        assert_eq!(merged, blobs);

        assert!(metrics.dispatch_send_wait_ns.load(Relaxed) > 0);
        assert!(metrics.result_recv_wait_ns.load(Relaxed) > 0);
        assert_eq!(metrics.desc_lock_wait_ns.load(Relaxed), 0);

        let mut names = Vec::new();
        metrics.for_each(|name, _| names.push(name));
        assert_eq!(
            names,
            [
                "classify_unordered_dispatch_send_wait_ns",
                "classify_unordered_desc_lock_wait_ns",
                "classify_unordered_desc_recv_wait_ns",
                "classify_unordered_result_send_wait_ns",
                "classify_unordered_result_recv_wait_ns",
            ]
        );
    }

    /// The ordered phase merges in schedule order whatever the completion
    /// order, passes each entry's position through, and never lets
    /// classification run more than `window` blobs past the earliest
    /// unmerged one.
    #[test]
    fn ordered_phase_merges_in_schedule_order_within_its_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 40;
        const WINDOW: usize = 4;
        let path = write_many_blob_pbf(dir.path(), BLOBS);
        let (schedule, shared_file) = super::build_classify_schedule(
            &path,
            None,
            UNCHECKED,
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(schedule.len(), BLOBS);

        let classified = AtomicUsize::new(0);
        let mut merged = Vec::new();
        super::parallel_classify_phase_ordered(
            &shared_file,
            &schedule,
            Some(3),
            std::num::NonZeroUsize::new(WINDOW),
            || (),
            |_block, ()| {
                classified.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            |seq, ()| {
                // Every classified blob was admitted while fewer than
                // `merged.len() + WINDOW` positions were open.
                let seen = classified.load(Ordering::Relaxed);
                assert!(
                    seen <= merged.len() + WINDOW,
                    "{seen} blobs classified with {} merged and a window of {WINDOW}",
                    merged.len()
                );
                merged.push(seq);
                Ok(())
            },
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(merged, (0..BLOBS).collect::<Vec<_>>());
    }

    /// Every pool of this module locates a failure of the work on one blob
    /// with the input's path, the blob's ordinal in the file and its frame
    /// offset - the location the header walker stamps - and leaves a
    /// cancellation unlocated.
    #[test]
    #[allow(clippy::too_many_lines)] // one case per pool, each with its own setup
    fn every_pool_locates_a_blob_failure_with_file_ordinal_and_offset() {
        let dir = crate::path_guard::tests::scratch();
        const BLOBS: usize = 6;
        let path = write_many_blob_pbf(dir.path(), BLOBS);
        let host = crate::host::Host::entered_for_tests();
        let (schedule, input) =
            super::build_classify_schedule(&path, None, UNCHECKED, &host).unwrap();
        assert_eq!(schedule.len(), BLOBS);
        // Fail on the blob holding node 4: schedule position 3, file blob 4
        // (blob 0 is the OSMHeader).
        let target = schedule[3];
        assert_eq!(target.blob_index(), Some(4));
        let fails_on_target = |block: &crate::PrimitiveBlock| -> crate::Result<()> {
            let hit = block.elements_skip_metadata().any(|e| match e {
                crate::Element::Node(n) => n.id() == 4,
                crate::Element::DenseNode(n) => n.id() == 4,
                _ => false,
            });
            if hit {
                Err(crate::Error::invalid_data("forced failure"))
            } else {
                Ok(())
            }
        };
        let check = |name: &str, err: crate::Error| {
            assert_eq!(err.source_path(), Some(path.as_path()), "{name}: {err}");
            assert_eq!(err.blob_index(), Some(4), "{name}: {err}");
            assert_eq!(
                err.byte_offset().map(|o| o.0),
                Some(target.frame_start()),
                "{name}: {err}"
            );
            let text = err.report();
            assert!(
                text.contains(&path.display().to_string()) && text.contains("blob 4"),
                "{name}: {text}"
            );
        };

        let err = super::parallel_classify_phase(
            &input,
            &schedule,
            Some(2),
            || (),
            |block, ()| fails_on_target(block),
            |_, ()| {},
            &host,
        )
        .unwrap_err();
        check("phase", err);

        let err = super::parallel_classify_phase_ordered(
            &input,
            &schedule,
            Some(2),
            std::num::NonZeroUsize::new(2),
            || (),
            |block, ()| fails_on_target(block),
            |_, ()| Ok(()),
            &host,
        )
        .unwrap_err();
        check("ordered", err);

        let err = super::parallel_classify_accumulate(
            &input,
            &schedule,
            Some(2),
            || (),
            |block, ()| fails_on_target(block),
            |()| {},
            &host,
        )
        .unwrap_err();
        check("accumulate", err);

        // The raw pool hands over bytes; fail on the target's position by
        // the payload length the schedule recorded for it.
        let err = super::parallel_scan_blobs_raw(
            &input,
            &schedule[3..4],
            Some(1),
            || (),
            |_position, _bytes, ()| -> crate::Result<()> {
                Err(crate::Error::invalid_data("forced failure"))
            },
            |_, ()| {},
            &host,
        )
        .unwrap_err();
        check("raw", err);

        // A failed pread of a payload that is not there: the I/O error is
        // located the same way.
        let missing = [ScheduledBlob::at(1 << 40, 16)];
        let err = super::parallel_classify_phase(
            &input,
            &missing,
            Some(1),
            || (),
            |_block, ()| Ok(()),
            |_, ()| {},
            &host,
        )
        .unwrap_err();
        assert_eq!(err.source_path(), Some(path.as_path()), "{err}");
        assert_eq!(err.byte_offset().map(|o| o.0), Some((1 << 40) - 4), "{err}");

        // A cancellation is not a fault of the input.
        let cancelled = crate::host::Host::entered_for_tests();
        let err = super::parallel_classify_phase(
            &input,
            &schedule,
            Some(1),
            || (),
            |_block, ()| -> crate::Result<()> {
                cancelled.invocation.cancel();
                cancelled.invocation.check()
            },
            |_, ()| {},
            &host,
        )
        .unwrap_err();
        assert!(err.is_cancelled());
        assert_eq!(err.source_path(), None, "{err}");
    }

    #[test]
    fn willneed_prefetch_defaults_to_coalesced_schedule() {
        let schedule = [ScheduledBlob::at(100, 20), ScheduledBlob::at(144, 5)];

        let prefetch = WillneedPrefetch::new(&schedule, &crate::host::Call::detached());

        assert_eq!(prefetch.ranges, vec![(100, 49)]);
        assert_eq!(prefetch.next_range, 0);
        assert_eq!(prefetch.next_offset, 0);
    }

    #[test]
    fn willneed_ranges_coalesce_abutting_body_ranges() {
        let schedule = [
            ScheduledBlob::at(100, 20),
            ScheduledBlob::at(120, 30),
            ScheduledBlob::at(5_000, 5),
        ];

        assert_eq!(
            coalesce_willneed_ranges(&schedule),
            vec![(100, 50), (5_000, 5)]
        );
    }

    #[test]
    fn willneed_ranges_coalesce_realistic_blob_header_gaps() {
        let schedule = [
            ScheduledBlob::at(100, 20),
            // Real PBF payloads have a four-byte header length plus the
            // BlobHeader between bodies. These represent 24- and 4096-byte
            // gaps, both intentionally bridged by the prefetch range.
            ScheduledBlob::at(144, 5),
            ScheduledBlob::at(4_245, 10),
            ScheduledBlob::at(8_352, 10),
        ];

        assert_eq!(
            coalesce_willneed_ranges(&schedule),
            vec![(100, 4_155), (8_352, 10)]
        );
    }
}
