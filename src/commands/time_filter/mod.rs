//! Filter history PBF to a snapshot at a cutoff timestamp.
//!
//! Keeps, for each object ID, the latest version with `timestamp <= cutoff`.
//! If that latest version is deleted (`visible=false`), the object is omitted.
//!
//! Two paths, picked from the header's `HistoricalInformation` required
//! feature flag:
//! - **History input**: sequential pending-group state machine on a
//!   parallel-decode reader. Version selection needs cross-element peek,
//!   and within-group versions can straddle blob boundaries, which rules
//!   out trivial per-block parallelism.
//! - **Snapshot input**: parallel per-block filter. Each (kind, id) appears
//!   in exactly one block, so blocks are independent.
//!
//! Neither path reads blob indexdata: every data blob is decoded and its
//! elements are handled by their actual kind and ID, and the output's
//! indexdata is built fresh. So time-filter has no `--force`, accepts a
//! wholly unindexed input as it accepts an indexed one, and believes no
//! sidecar claim. A partially indexed input is refused on both paths, as by
//! every command (the reader and the schedule walk refuse it once both
//! kinds of data blob have been seen).
//! It does believe two header declarations: `Sort.Type_then_ID` (required;
//! the history path's grouping needs each object's versions contiguous) and
//! `HistoricalInformation` (which picks the path).

use super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_sorted, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{
    OwnedElement, dense_node_metadata, element_metadata, owned_to_metadata, read_dense_node,
    read_node, read_relation, read_way, relation_members_into,
};
use crate::read::input::{InputSource, PinnedInput};
use crate::scan::classify::{build_classify_schedule, parallel_classify_phase_ordered};
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter};
use crate::{DenseNode, Element, ElementKind, ElementReader, Node, PrimitiveBlock, Relation, Way};

/// Statistics from a `time-filter` snapshot operation.
pub struct TimeFilterStats {
    pub versions_seen: u64,
    pub versions_before_cutoff: u64,
    pub elements_written: u64,
    pub dropped_deleted: u64,
    pub dropped_no_snapshot_version: u64,
}

impl TimeFilterStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "time-filter: {} versions scanned, {} <= cutoff, {} elements written ({} deleted at cutoff, {} without version <= cutoff)",
            self.versions_seen,
            self.versions_before_cutoff,
            self.elements_written,
            self.dropped_deleted,
            self.dropped_no_snapshot_version,
        )?;

        Ok(())
    }
}

struct PendingGroup {
    kind: ElementKind,
    id: i64,
    latest: Option<OwnedElement>,
}

/// Filter history PBF to a snapshot at `cutoff_timestamp` (UNIX seconds).
///
/// Requires sorted input (`Sort.Type_then_ID`) so each object's versions are
/// contiguous and the snapshot can be computed in one streaming pass.
///
/// A snapshot input is read in two passes, and which path runs is only known
/// once the header is read, so the input must be a regular file or a block
/// device on either path (`InvalidArgument` otherwise), and is pinned for the
/// run (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin
/// is used as it is. Where a later open cannot reach the pinned file, an
/// input replaced mid-run fails with `InputReplaced` and nothing is
/// published.
pub fn time_filter<'a>(
    input: impl Into<InputSource<'a>>,
    output: crate::output::Output,
    cutoff_timestamp: i64,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TimeFilterStats> {
    host.invocation.check()?;
    let result = time_filter_to(
        input.into(),
        &output,
        cutoff_timestamp,
        write,
        overrides,
        host,
    )?;
    output.publish(&host.invocation)?;
    Ok(result)
}

/// [`time_filter`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn time_filter_to(
    input: InputSource<'_>,
    output: &crate::output::Output,
    cutoff_timestamp: i64,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<TimeFilterStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    // The snapshot path opens the input twice, each through its own open
    // file description: the header reader below, then the schedule walk
    // whose descriptor the classify pool preads. The history path reads
    // everything through the header reader. Both opens are derived from this
    // one pin, so a file renamed over the input mid-run cannot have its
    // header (which picks the path) read from one file and its blobs
    // filtered from the other: where `/proc/self/fd` reopens the pin every
    // open reaches the pinned file, otherwise on Unix a replaced input fails
    // the run (`PinnedInput` says what it does off Unix). Taken before any
    // other open of the input, so on the history path too: which path runs
    // is learned from the header, so whether the input is reopened is not
    // known until it has been read once, and a FIFO is refused up front.
    let pin = input.pin(host)?;
    // History path: no arena-cap hook. Snapshot path: invokes the
    // host's `cross_thread_alloc_pool` hook - see the in-function
    // comment in time_filter_snapshot.
    let reader = ElementReader::open_in(&pin, direct_io, host)?;
    require_sorted(
        reader.header().is_sorted(),
        pin.path(),
        "Input history PBF",
        "Time filtering requires sorted input to preserve type-then-ID ordering while selecting the snapshot.",
    )?;

    let is_history = reader.header().has_historical_information();
    let mut writer = writer_from_header(
        output,
        compression,
        reader.header(),
        &super::output_header::OutputFacts {
            replication: super::output_header::Replication::AsOf(cutoff_timestamp),
            ..super::output_header::OutputFacts::carry(super::output_header::Ways::Reencoded)
        },
        overrides,
        backend,
        host,
    )?;

    let stats = if is_history {
        time_filter_history(reader, &mut writer, cutoff_timestamp, &host.invocation)?
    } else {
        // Drop the reader before the snapshot path - the migrated
        // snapshot path opens the input via HeaderWalker through
        // build_classify_schedule, not ElementReader, so holding the
        // reader open here would just keep an extra fd around.
        drop(reader);
        time_filter_snapshot(&pin, &mut writer, cutoff_timestamp, compression, host)?
    };

    writer.flush()?;
    host.invocation
        .counter("timefilter_versions_seen", stats.versions_seen);
    host.invocation.counter(
        "timefilter_versions_before_cutoff",
        stats.versions_before_cutoff,
    );
    host.invocation
        .counter("timefilter_elements_written", stats.elements_written);
    host.invocation
        .counter("timefilter_dropped_deleted", stats.dropped_deleted);
    host.invocation.counter(
        "timefilter_dropped_no_snapshot_version",
        stats.dropped_no_snapshot_version,
    );
    host.invocation
        .counter("timefilter_is_history_path", is_history);
    Ok(stats)
}

// ---------------------------------------------------------------------------
// History path: pending-group state machine on a parallel-decode reader.
// ---------------------------------------------------------------------------

#[hotpath::measure]
fn time_filter_history(
    reader: ElementReader<crate::file_reader::FileReader>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
    invocation: &crate::host::Invocation,
) -> Result<TimeFilterStats> {
    let mut bb = BlockBuilder::new();
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };
    let mut pending: Option<PendingGroup> = None;
    let mut flush_error: Result<()> = Ok(());

    invocation.marker("TIMEFILTER_HISTORY_START");
    // Parallel decode via the 3-stage pipelined reader (IO -> rayon decode ->
    // reorder). Element order across blocks is preserved by the reorder
    // stage, which is load-bearing: the pending-group state machine below
    // depends on sorted traversal to know when a group ends. Re-encode and
    // group selection run sequentially on this thread.
    //
    // Hot-path timing lives in `#[cfg_attr(feature = "hotpath",
    // hotpath::measure)]` on the inner helpers - run with `--hotpath` for a
    // per-function breakdown. Release builds pay zero cost for that
    // attribute. Don't re-add per-element Instant::now() here: Japan scale
    // is 344 M elements and the time-source overhead alone doubled wall in
    // an earlier iteration of this instrumentation.
    reader.for_each_pipelined(|element| {
        if flush_error.is_err() {
            return;
        }
        let (kind, id, timestamp) = match element_identity_and_timestamp(&element) {
            Ok(v) => v,
            Err(e) => {
                flush_error = Err(e);
                return;
            }
        };
        stats.versions_seen += 1;

        let group_changed = pending
            .as_ref()
            .is_some_and(|g| g.kind != kind || g.id != id);
        if group_changed
            && let Some(group) = pending.take()
            && let Err(e) = flush_group(group, &mut bb, writer, &mut stats)
        {
            flush_error = Err(e);
            return;
        }

        if pending.is_none() {
            pending = Some(PendingGroup {
                kind,
                id,
                latest: None,
            });
        }

        // Unconditional overwrite on each matching version is correct
        // under the OSM history-file convention: versions within a
        // `(kind, id)` group arrive in ascending-version order (the
        // same convention PBF history files ship with). The "latest
        // element whose timestamp <= cutoff" is therefore the last one
        // we see during the forward scan. A malformed history file
        // with out-of-order versions would violate this, but that is
        // out of spec.
        if timestamp <= cutoff_timestamp {
            stats.versions_before_cutoff += 1;
            if let Some(group) = pending.as_mut() {
                match clone_owned_element(&element) {
                    Ok(owned) => group.latest = Some(owned),
                    Err(e) => flush_error = Err(e),
                }
            }
        }
    })?;
    flush_error?;

    if let Some(group) = pending.take() {
        flush_group(group, &mut bb, writer, &mut stats)?;
    }
    if let Some(block) = bb.take_owned()? {
        writer.write_primitive_block_owned(block)?;
    }
    invocation.marker("TIMEFILTER_HISTORY_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Snapshot path: parallel per-block filter.
// ---------------------------------------------------------------------------
//
// For non-history input each (kind, id) appears exactly once in the input,
// so blocks are independent: no cross-block group straddling, no version
// selection, no clone-into-OwnedElement on the consumer thread. Each worker
// iterates its block, drops elements whose timestamp > cutoff or whose
// visible=false (rare in snapshot inputs but the field can be set
// explicitly), and writes surviving elements straight into a local
// BlockBuilder via reference. Consumer drains batch results in order.

/// Snapshot path migrated 2026-04-28 from the
/// `for_each_primitive_block_batch_budgeted` + `par_iter().map(thread_local
/// BB)` + drain shape to `parallel_classify_phase` + `ReorderBuffer` (since
/// replaced by `parallel_classify_phase_ordered`, whose window bounds what the
/// hand-kept reorder buffer did not). The
/// pre-migration shape hit a structural ~28 GB anon ceiling at planet (five
/// SIGKILL'd attempts, last three with full instrumentation) because the
/// `par_iter().collect()` step materialises the full batch's
/// `Vec<OwnedBlockTriple>` results before draining, and the upstream pipeline
/// can hold its bounded decode window of additional PrimitiveBlocks in flight
/// while the batch drains.
/// Allocator knobs (`malloc_trim` per batch, `M_MMAP_THRESHOLD=64K`,
/// `decode_ahead=8`) all hit the same ceiling: mallinfo2 showed bytes
/// shifting between `arena` and `hblkhd` but the total live set was
/// genuine working set, not retention. The migration mirrors the
/// `cat --clean` (`b347c0a`, 28.9 GB -> 750 MB) and `check --ids`
/// (`516129e`, 29.2 GB -> 504 MB) precedents.
#[hotpath::measure]
fn time_filter_snapshot(
    input: &PinnedInput,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
    compression: Compression,
    host: &crate::host::Host,
) -> Result<TimeFilterStats> {
    // Per-blob worker pool; same precedent as cat --clean and
    // check --refs / verify_ids. Post-migration, each blob's BlockBuilder
    // alloc/free cycle is confined to a single worker thread, so the
    // pattern that regressed iter-3 (cross-blob scratch reuse defeated
    // by arena capping) doesn't apply: K=2 measured -69 % wall and
    // -24 % anon on the pre-migration code, but that pre-migration code
    // had thread-local BlockBuilder state surviving across blobs on the
    // same rayon worker. The classify pool's workers hold no builder across
    // blobs here.
    host.process.cross_thread_alloc_pool();

    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };

    host.invocation.marker("TIMEFILTER_SNAPSHOT_START");
    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; these samples are inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "timefilter_snapshot_start_mallinfo");

    // No kind or blob filter: every OsmData blob is scheduled whatever its
    // indexdata says or lacks, and `filter_block_snapshot` works on the
    // decoded elements by their actual kind. A partially indexed file is
    // refused by the walk all the same, as by every command-input read
    // (reference/indexdata-trust.md, "Missing and partial indexdata").
    let (schedule, shared_file) = build_classify_schedule(input, None, host)?;

    if schedule.is_empty() {
        host.invocation.marker("TIMEFILTER_SNAPSHOT_END");
        return Ok(stats);
    }

    type PhaseResult = crate::Result<(Vec<Vec<u8>>, TimeFilterStats)>;

    // A fresh BlockBuilder per blob (BlockBuilder::new is just a few empty
    // Vec/HashMap initialisers; no heap reservation until elements are
    // added), deliberately: see the function doc on builders that survive
    // across blobs. Same pattern as cat --clean's worker. The ordered pool's
    // window bounds the framed results alive between decode and write.
    parallel_classify_phase_ordered(
        &shared_file,
        &schedule,
        None,
        None,
        || (),
        |block, _state| -> PhaseResult {
            let mut bb = BlockBuilder::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let block_stats = filter_block_snapshot(block, cutoff_timestamp, &mut bb, &mut output)?;
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<Vec<u8>> = Vec::with_capacity(output.len());
            for OwnedBlock {
                bytes: block_bytes,
                index,
                tagdata,
                way_members,
            } in output
            {
                let indexdata = index.serialize();
                let blob = crate::writer::frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                    way_members.as_deref(),
                    &host.invocation.metrics().writer,
                )?;
                framed.push(blob.into_vec());
            }
            Ok((framed, block_stats))
        },
        |_position, (framed, block_stats)| {
            // Called in schedule order; a write error ends the pass at once.
            // A failed blob ends it too, located with its input.
            stats.versions_seen += block_stats.versions_seen;
            stats.versions_before_cutoff += block_stats.versions_before_cutoff;
            stats.elements_written += block_stats.elements_written;
            stats.dropped_deleted += block_stats.dropped_deleted;
            stats.dropped_no_snapshot_version += block_stats.dropped_no_snapshot_version;
            for blob in framed {
                writer.write_raw_owned(blob)?;
            }
            Ok(())
        },
        host,
    )?;

    // Brokkr stores named counters generically and reports changes in sidecar
    // comparisons; this sample is inert unless the invocation has a sink.
    crate::debug::emit_mallinfo2(&host.invocation, "timefilter_snapshot_end_mallinfo");
    host.invocation.marker("TIMEFILTER_SNAPSHOT_END");
    Ok(stats)
}

/// Snapshot-gate: returns `Some(())` if the element survives both the cutoff
/// timestamp check and the visibility check. Updates `stats` for the drop
/// counters along the way. Returning `None` means the caller should `continue`.
fn snapshot_gate(
    ts: Option<i64>,
    ident: (&str, i64),
    visible: bool,
    cutoff_timestamp: i64,
    stats: &mut TimeFilterStats,
) -> crate::Result<Option<()>> {
    // An element with no timestamp is not an element from before the
    // cutoff. See the timestamp section at the bottom of this file.
    let ts = require_timestamp(ts, ident.0, ident.1)?;
    if ts > cutoff_timestamp {
        stats.dropped_no_snapshot_version += 1;
        return Ok(None);
    }
    stats.versions_before_cutoff += 1;
    if !visible {
        stats.dropped_deleted += 1;
        return Ok(None);
    }
    Ok(Some(()))
}

#[hotpath::measure]
#[allow(clippy::too_many_lines)]
fn filter_block_snapshot(
    block: &PrimitiveBlock,
    cutoff_timestamp: i64,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<TimeFilterStats> {
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        stats.versions_seen += 1;
        match &element {
            Element::DenseNode(dn) => {
                let Some(()) = snapshot_gate(
                    dense_timestamp(dn),
                    ("node", dn.id()),
                    dense_visible(dn),
                    cutoff_timestamp,
                    &mut stats,
                )?
                else {
                    continue;
                };
                ensure_node_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(dn.tags());
                let meta = dense_node_metadata(dn)?;
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    tags_buf.iter().copied(),
                    meta.as_ref(),
                );
                stats.elements_written += 1;
            }
            Element::Node(n) => {
                let Some(()) = snapshot_gate(
                    node_timestamp(n),
                    ("node", n.id()),
                    node_visible(n),
                    cutoff_timestamp,
                    &mut stats,
                )?
                else {
                    continue;
                };
                ensure_node_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(n.tags());
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    tags_buf.iter().copied(),
                    meta.as_ref(),
                );
                stats.elements_written += 1;
            }
            Element::Way(w) => {
                let Some(()) = snapshot_gate(
                    way_timestamp(w),
                    ("way", w.id()),
                    way_visible(w),
                    cutoff_timestamp,
                    &mut stats,
                )?
                else {
                    continue;
                };
                ensure_way_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
                bb.add_way(w.id(), tags_buf.iter().copied(), &refs_buf, meta.as_ref());
                stats.elements_written += 1;
            }
            Element::Relation(r) => {
                let Some(()) = snapshot_gate(
                    relation_timestamp(r),
                    ("relation", r.id()),
                    relation_visible(r),
                    cutoff_timestamp,
                    &mut stats,
                )?
                else {
                    continue;
                };
                ensure_relation_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(r.tags());
                relation_members_into(r, &mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation(
                    r.id(),
                    tags_buf.iter().copied(),
                    &members_buf,
                    meta.as_ref(),
                );
                stats.elements_written += 1;
            }
        }
    }
    Ok(stats)
}

fn dense_visible(dn: &DenseNode<'_>) -> bool {
    #[allow(clippy::redundant_closure_for_method_calls)]
    dn.info().is_none_or(|i| i.visible())
}

fn node_visible(n: &Node<'_>) -> bool {
    n.info().visible()
}

fn way_visible(w: &Way<'_>) -> bool {
    w.info().visible()
}

fn relation_visible(r: &Relation<'_>) -> bool {
    r.info().visible()
}

#[hotpath::measure]
fn flush_group(
    group: PendingGroup,
    bb: &mut BlockBuilder,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut TimeFilterStats,
) -> Result<()> {
    match group.latest {
        None => {
            stats.dropped_no_snapshot_version += 1;
        }
        Some(owned) if !owned.visible() => {
            stats.dropped_deleted += 1;
        }
        Some(owned) => {
            write_owned_element(bb, writer, owned)?;
            stats.elements_written += 1;
        }
    }
    Ok(())
}

#[hotpath::measure]
fn write_owned_element(
    bb: &mut BlockBuilder,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    elem: OwnedElement,
) -> Result<()> {
    match elem {
        OwnedElement::Node(n) => {
            if !bb.can_add_node()
                && let Some(block) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(block)?;
            }
            let meta = owned_to_metadata(n.metadata.as_ref());
            bb.add_node(
                n.id,
                n.decimicro_lat,
                n.decimicro_lon,
                n.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                meta.as_ref(),
            );
        }
        OwnedElement::Way(w) => {
            if !bb.can_add_way()
                && let Some(block) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(block)?;
            }
            let meta = owned_to_metadata(w.metadata.as_ref());
            bb.add_way(
                w.id,
                w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                &w.refs,
                meta.as_ref(),
            );
        }
        OwnedElement::Relation(r) => {
            if !bb.can_add_relation()
                && let Some(block) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(block)?;
            }
            let meta = owned_to_metadata(r.metadata.as_ref());
            let members: Vec<MemberData<'_>> = r
                .members
                .iter()
                .map(|m| MemberData {
                    id: m.id,
                    role: &m.role,
                })
                .collect();
            bb.add_relation(
                r.id,
                r.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
                &members,
                meta.as_ref(),
            );
        }
    }
    Ok(())
}

fn element_identity_and_timestamp(element: &Element<'_>) -> crate::Result<(ElementKind, i64, i64)> {
    let (kind, id, ts) = match element {
        Element::DenseNode(dn) => (ElementKind::Node, dn.id(), dense_timestamp(dn)),
        Element::Node(n) => (ElementKind::Node, n.id(), node_timestamp(n)),
        Element::Way(w) => (ElementKind::Way, w.id(), way_timestamp(w)),
        Element::Relation(r) => (ElementKind::Relation, r.id(), relation_timestamp(r)),
    };
    Ok((kind, id, require_timestamp(ts, kind.as_str(), id)?))
}

// ---------------------------------------------------------------------------
// Timestamps
//
// Absent is not the epoch. Every one of these used to end in
// `unwrap_or(0)`, so an element with no `Info` - or an `Info` with no
// timestamp field, which is what `osmium --drop-metadata` and several
// pbfhogg paths produce - reported timestamp 0. Zero is at or below every
// plausible cutoff, so `snapshot_gate` kept the element and counted it in
// `versions_before_cutoff`, and `time-filter` printed "N versions scanned,
// N <= cutoff, N elements written" having filtered nothing at all. A
// command that reports success having done nothing is the worst shape of
// bug in this domain, so a metadata-stripped input is now a hard error
// naming the element.
// ---------------------------------------------------------------------------

/// `None` means the element carries no timestamp at all.
fn dense_timestamp(dn: &DenseNode<'_>) -> Option<i64> {
    // `DenseNodeInfo::milli_timestamp` is infallible: DenseInfo carries a
    // parallel timestamp array, so if the block has DenseInfo at all it
    // has a timestamp for this node. Absence is `info()` returning None,
    // which is exactly what a metadata-stripped dense block looks like.
    Some(dn.info()?.milli_timestamp() / 1000)
}

fn node_timestamp(n: &Node<'_>) -> Option<i64> {
    Some(n.info().milli_timestamp()? / 1000)
}

fn way_timestamp(w: &Way<'_>) -> Option<i64> {
    Some(w.info().milli_timestamp()? / 1000)
}

fn relation_timestamp(r: &Relation<'_>) -> Option<i64> {
    Some(r.info().milli_timestamp()? / 1000)
}

/// Turn an absent timestamp into the error that names it.
fn require_timestamp(ts: Option<i64>, kind: &str, id: i64) -> crate::Result<i64> {
    ts.ok_or_else(|| {
        crate::Error::invalid_data(format!(
            "time-filter: {kind} {id} carries no timestamp. Filtering by time needs one, and \
             treating an absent timestamp as the Unix epoch would place every such element at \
             or below any cutoff - so the command would keep the entire input, write it out \
             unchanged, and report success having filtered nothing. The input has had its \
             metadata stripped (osmium --drop-metadata and several pbfhogg write paths do \
             this); re-run time-filter against an input that still carries element metadata."
        ))
    })
}

#[hotpath::measure]
fn clone_owned_element(element: &Element<'_>) -> crate::Result<OwnedElement> {
    Ok(match element {
        Element::DenseNode(dn) => OwnedElement::Node(read_dense_node(dn)?),
        Element::Node(n) => OwnedElement::Node(read_node(n)?),
        Element::Way(w) => OwnedElement::Way(read_way(w)?),
        Element::Relation(r) => OwnedElement::Relation(read_relation(r)?),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::blob_meta::BlobIndex;
    use crate::elements::MemberId;

    const CUTOFF: i64 = 250;

    /// What a fixture blob's header says about it.
    #[derive(Clone, Copy, Debug)]
    enum Claim {
        /// Indexdata scanned from the payload.
        Honest,
        /// No indexdata.
        Absent,
        /// Parseable indexdata that is false on every field: another kind,
        /// a range holding none of its IDs and a wrong count.
        False,
    }

    /// One fixture element: kind, ID, version, timestamp, visible.
    type Version = (char, i64, i32, i64, bool);

    /// The history fixture's blobs. Node 2 and way 11 straddle a blob
    /// boundary with versions on both sides of the cutoff; node 3 is deleted
    /// at the cutoff, node 4 has no version at or before it, and way 10 and
    /// relation 100 have a later version past it.
    const HISTORY: &[&[Version]] = &[
        &[
            ('n', 1, 1, 100, true),
            ('n', 2, 1, 100, true),
            ('n', 2, 2, 200, true),
        ],
        &[
            ('n', 2, 3, 300, true),
            ('n', 3, 1, 100, true),
            ('n', 3, 2, 150, false),
            ('n', 4, 1, 400, true),
        ],
        &[
            ('w', 10, 1, 100, true),
            ('w', 10, 2, 300, true),
            ('w', 11, 1, 200, true),
        ],
        &[('w', 11, 2, 240, true)],
        &[('r', 100, 1, 100, true), ('r', 100, 2, 260, true)],
    ];

    /// The snapshot fixture's blobs: each object once. Node 2 and way 11 are
    /// past the cutoff and node 3 is deleted.
    const SNAPSHOT: &[&[Version]] = &[
        &[('n', 1, 1, 100, true), ('n', 2, 1, 300, true)],
        &[('n', 3, 2, 100, false), ('n', 4, 1, 200, true)],
        &[('w', 10, 1, 100, true)],
        &[('w', 11, 1, 300, true)],
        &[('r', 100, 1, 100, true)],
    ];

    fn write_fixture(path: &Path, history: bool, claims: &[Claim]) {
        let blobs = if history { HISTORY } else { SNAPSHOT };
        assert_eq!(blobs.len(), claims.len());
        let file = std::fs::File::create(path).unwrap();
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        let mut header = crate::block_builder::HeaderBuilder::new().sorted();
        if history {
            header = header.historical();
        }
        writer.write_header(&header.build().unwrap()).unwrap();
        let mut bb = BlockBuilder::new();
        for (blob, claim) in blobs.iter().zip(claims) {
            for &(kind, id, version, timestamp, visible) in *blob {
                let meta = crate::block_builder::Metadata {
                    version,
                    timestamp,
                    changeset: 7,
                    uid: 3,
                    user: "u",
                    visible,
                };
                let tags = [("name", "x")];
                match kind {
                    'n' => bb.add_node(id, 10, 10, tags, Some(&meta)),
                    'w' => bb.add_way(id, tags, &[1, 2], Some(&meta)),
                    _ => bb.add_relation(
                        id,
                        tags,
                        &[MemberData {
                            id: MemberId::Way(10),
                            role: "outer",
                        }],
                        Some(&meta),
                    ),
                }
            }
            let bytes = bb.take().unwrap().unwrap();
            match claim {
                Claim::Honest => writer.write_primitive_block(bytes).unwrap(),
                Claim::Absent => writer.write_primitive_block_no_indexdata(bytes).unwrap(),
                Claim::False => {
                    let kind = match blob[0].0 {
                        'n' => ElementKind::Relation,
                        'w' => ElementKind::Node,
                        _ => ElementKind::Way,
                    };
                    let index = BlobIndex {
                        kind,
                        min_id: 1000,
                        max_id: 2000,
                        count: 99,
                        bbox: None,
                    };
                    writer
                        .write_primitive_block_owned(OwnedBlock {
                            bytes: bytes.to_vec(),
                            index,
                            tagdata: None,
                            way_members: None,
                        })
                        .unwrap();
                }
            }
        }
        writer.flush().unwrap();
    }

    /// An output element: kind, ID, version.
    type Written = (char, i64, i32);

    /// One run: output bytes, the written elements in file order, and stats.
    #[expect(
        clippy::unwrap_in_result,
        reason = "test helper: the command's result is what is under test, the fixture I/O \
                  around it is not"
    )]
    fn run(input: &Path) -> crate::Result<(Vec<u8>, Vec<Written>, [u64; 5])> {
        let dir = crate::path_guard::tests::scratch();
        let output = dir.path().join("out.pbf");
        let staged = crate::output::Output::new(
            &output,
            crate::output::Overwrite::Refuse,
            &crate::host::Diagnostics::discard(),
        )
        .unwrap();
        let host = crate::host::Host::for_tests();
        let s = time_filter(
            input,
            staged,
            CUTOFF,
            &WriteOptions::default(),
            &HeaderOverrides::default(),
            &host,
        )?;
        let mut elements = Vec::new();
        ElementReader::open_in(&output, false, &host)
            .unwrap()
            .for_each(|element| {
                elements.push(match element {
                    Element::DenseNode(n) => ('n', n.id(), n.info().unwrap().version()),
                    Element::Node(n) => ('n', n.id(), n.info().version().unwrap()),
                    Element::Way(w) => ('w', w.id(), w.info().version().unwrap()),
                    Element::Relation(r) => ('r', r.id(), r.info().version().unwrap()),
                });
            })
            .unwrap();
        let stats = [
            s.versions_seen,
            s.versions_before_cutoff,
            s.elements_written,
            s.dropped_deleted,
            s.dropped_no_snapshot_version,
        ];
        Ok((std::fs::read(&output).unwrap(), elements, stats))
    }

    /// Neither path reads indexdata, so an input whose indexdata is false on
    /// every field, or absent on every blob, gives the wholly indexed file's
    /// output byte for byte, with the same stats. (A partially indexed input
    /// is refused on both paths mid-write, which this crate's test builds
    /// assert on as a writer dropped unflushed; the CLI input-contract table
    /// pins that refusal, in both orders.)
    #[test]
    fn indexdata_never_changes_the_output() {
        use Claim::{Absent as A, False as F, Honest as H};
        let dir = crate::path_guard::tests::scratch();
        let history_expected: &[Written] = &[
            ('n', 1, 1),
            ('n', 2, 2),
            ('w', 10, 1),
            ('w', 11, 2),
            ('r', 100, 1),
        ];
        let snapshot_expected: &[Written] =
            &[('n', 1, 1), ('n', 4, 1), ('w', 10, 1), ('r', 100, 1)];
        for (history, expected, expected_stats) in [
            (true, history_expected, [13, 9, 5, 1, 1]),
            (false, snapshot_expected, [7, 5, 4, 1, 2]),
        ] {
            let whole = dir.path().join(format!("whole-{history}.pbf"));
            write_fixture(&whole, history, &[H; 5]);
            let reference = run(&whole).unwrap();
            assert_eq!(reference.1, expected, "reference, history {history}");
            assert_eq!(
                reference.2, expected_stats,
                "reference stats, history {history}"
            );
            for (name, claims) in [("all false", [F; 5]), ("all absent", [A; 5])] {
                let what = format!("{name}, history {history}");
                let input = dir.path().join(format!("{name}-{history}.pbf"));
                write_fixture(&input, history, &claims);
                let got = run(&input).unwrap();
                assert_eq!(got.1, expected, "{what}");
                assert_eq!(got.2, reference.2, "stats: {what}");
                assert!(got.0 == reference.0, "output bytes differ: {what}");
            }
        }
    }
}
