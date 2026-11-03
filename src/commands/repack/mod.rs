//! Re-encode a PBF with a configurable per-blob element cap.
//!
//! Primary consumer: the blob-density measurement matrix in
//! [`reference/blob-density.md`](../../../reference/blob-density.md). That
//! matrix needs same-corpus-different-encoding pairs to control for blob-
//! count effects independent of byte size; `repack` is the only way to
//! produce them.
//!
//! Implementation: parallel three-phase scan (nodes, ways, relations).
//! Each worker decodes one input blob and filters it to the current kind.
//! What happens to its `M` matching elements depends on `M` alone:
//!
//! - `M >= cap` (a shrink blob): the worker re-encodes every one through
//!   its own long-lived `BlockBuilder` and frames each block as it fills:
//!   `M / cap` full blocks, then the `M % cap` remainder as its own block.
//!   It ships only frames, so nothing of the blob's elements outlives the
//!   worker.
//! - `M < cap` (a grow blob, or none at all): the worker ships the elements
//!   as decoded `Owned*` data, because they always go into the merge
//!   thread's builder.
//!
//! The merge thread runs a single `BlockBuilder` per kind, configured with
//! the requested cap, and consumes results in schedule order from the
//! ordered classify pool. Grow blobs coalesce in it across input-blob
//! boundaries, so a cap larger than the input blobs fires correctly. Before
//! it writes a shrink blob's frames it flushes that builder, so the lower
//! IDs it holds precede the frames' higher ones and the output keeps
//! `Sort.Type_then_ID`. Every block the merge thread's builder completes is
//! handed to the pipelined writer, which frames it on its executor in
//! sequence order with the raw frames around it.
//!
//! So a shrink blob's remainder never coalesces with what follows it: the
//! output's blocks for that blob depend on that blob alone. Repacking the
//! output again at the same cap can therefore still merge such a remainder
//! with a following grow blob's elements; one pass does not promise the
//! tightest packing.
//!
//! The shrink rule keeps the merge thread off the critical path where the
//! input blobs are larger than the cap. It used to re-encode every
//! remainder itself from owned elements: at planet, relation blobs hold
//! about 31k relations each, the merge thread then ran the relation phase
//! at about 8 cores, and the results queued behind it held about 0.7 GB of
//! owned remainders at the phase's start (`reference/performance.md`,
//! "repack and degrade encode each blob on its worker").
//!
//! The `repack_input_blobs_coalesced` counter tracks how often a non-empty
//! grow payload extends a non-empty central builder. The "never fired"
//! warning fires only when the cap exceeds every kind's total element count
//! (every kind collapses to a single output blob).

use super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header,
};
use crate::ElementKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata, read_dense_node,
    read_node, read_relation, read_way, read_way_with_locations, relation_members_into,
    write_single_node_local, write_single_relation_local, write_single_way_local,
};
use crate::read::input::InputSource;
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter};
use crate::{Element, ElementReader};

/// Per-run statistics from a repack operation.
pub struct RepackStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub elements_per_blob: usize,
}

impl RepackStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Repacked {} elements into {} blobs (cap {} elements/blob)",
            self.elements_written, self.blobs_written, self.elements_per_blob,
        )?;

        Ok(())
    }
}

/// A grow blob's elements: 0 to `cap-1` of them, for the merge thread's
/// builder. One variant populated per phase.
enum KindPayload {
    Nodes(Vec<OwnedNode>),
    Ways(Vec<OwnedWay>),
    Relations(Vec<OwnedRelation>),
}

impl KindPayload {
    fn len(&self) -> u64 {
        let n = match self {
            Self::Nodes(v) => v.len(),
            Self::Ways(v) => v.len(),
            Self::Relations(v) => v.len(),
        };
        n as u64
    }
}

/// One worker's output for one input blob.
enum WorkerOutput {
    /// A shrink blob (at least `cap` matching elements), encoded whole by
    /// its worker: `elements / cap` blocks of `cap` elements, then the
    /// remainder as one block when `elements % cap != 0`.
    Framed { frames: Vec<Vec<u8>>, elements: u64 },
    /// A grow blob (fewer than `cap` matching elements, possibly none), for
    /// the merge thread's builder.
    Owned(KindPayload),
}

/// Re-encode `input` to `output` with a per-blob element cap of
/// `elements_per_blob`. Element semantics are preserved: every element
/// round-trips with its tags, refs, members, metadata, and DenseNodes
/// encoding. Output is type-sorted (nodes, then ways, then relations);
/// the `Sort.Type_then_ID` flag is propagated when the input has it.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin is
/// used as it is. Where a later open cannot reach the pinned file, an input
/// replaced mid-run fails with `InputReplaced` and nothing is published.
pub fn repack<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    elements_per_blob: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RepackStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = repack_to(
        input.into(),
        &output,
        elements_per_blob,
        write,
        overrides,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`repack`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn repack_to(
    input: InputSource<'_>,
    output: &crate::output::BoundOutput,
    cap: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<RepackStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *write;
    let elements_per_blob = cap.get();

    // The input is opened three times, each through its own open file
    // description: the indexdata probe, the header reader, then the schedule
    // walk whose descriptor every per-kind pool preads. All three are derived
    // from this one pin, and the size bound below is read from it, so a
    // file renamed over the input mid-run cannot be probed, headed and
    // re-encoded partly from each file: where `/proc/self/fd` reopens the pin
    // every open reaches the pinned file, otherwise on Unix a replaced input
    // fails the run (`PinnedInput` says what it does off Unix). Taken before
    // any other open of the input.
    let pin = input.pin(host)?;
    let input = &pin;

    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. repack uses the parallel \
         per-kind classify pipeline, which needs indexdata to build per-kind \
         blob schedules.",
        host,
    )?;

    // Unlike tags-filter / extract, repack round-trips way payloads with
    // their inline coordinates, so an input declaring LocationsOnWays keeps
    // it: the way phase threads the coordinates through both the worker
    // encode path and the merge thread's owned path. An input without
    // LocationsOnWays carries no coordinates and the output is the plain
    // shape (no implicit conversion). The prepass fields are not rewritten,
    // so their declarations go, with the shared warning.
    let (header, preserve_locations) = {
        let reader = ElementReader::open_in(input, direct_io, host)?;
        let preserve = reader.header().has_locations_on_ways();
        (reader.header().clone(), preserve)
    };
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_header::OutputFacts::carry(
            super::output_header::Ways::ReencodedWithLocations,
        ),
        overrides,
        backend,
        host,
    )?;

    // The third result sizes IdSets in check callers; repack only needs the
    // schedules. Each per-kind phase re-encodes only its own kind, so an
    // unindexed blob (only under --force) sits in all three and is decoded
    // once per phase.
    let (schedules, shared_file, _) = crate::scan::classify::build_classify_schedules_split(
        input,
        crate::scan::classify::KindClaims::Refuse,
        host,
    )?;
    let (node_schedule, way_schedule, rel_schedule) = schedules.into_replicated();

    host.invocation
        .counter("repack_input_blobs_nodes", node_schedule.len());
    host.invocation
        .counter("repack_input_blobs_ways", way_schedule.len());
    host.invocation
        .counter("repack_input_blobs_relations", rel_schedule.len());
    host.invocation.counter(
        "repack_input_blobs_total",
        node_schedule.len() + way_schedule.len() + rel_schedule.len(),
    );

    // Pre-allocation ceiling for every BlockBuilder in this run, derived from
    // the input rather than fixed: a file cannot deliver more elements than it
    // has bytes. See `BlockBuilder::with_element_cap_prealloc`. The size is
    // the pinned file's, and a failed stat of it fails the run: falling back
    // to an unbounded ceiling would drop the bound exactly when it is unknown.
    let prealloc_bound = usize::try_from(input.metadata()?.len()).unwrap_or(usize::MAX);

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;
    let mut total_coalesces: u64 = 0;
    let mut total_worker_full_blobs: u64 = 0;
    let mut total_worker_tail_blobs: u64 = 0;
    let mut total_central_blobs: u64 = 0;
    let mut any_cap_fired: bool = false;

    host.invocation.marker("REPACK_NODES_START");
    let node_stats = run_kind_phase(
        &shared_file,
        &node_schedule,
        ElementKind::Node,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    host.invocation.marker("REPACK_NODES_END");
    emit_phase_counters(ElementKind::Node, &node_stats, &host.invocation);
    blobs_written += node_stats.blobs;
    elements_written += node_stats.elements;
    total_coalesces += node_stats.coalesces;
    total_worker_full_blobs += node_stats.worker_full_blobs;
    total_worker_tail_blobs += node_stats.worker_tail_blobs;
    total_central_blobs += node_stats.central_blobs;
    any_cap_fired |= node_stats.cap_fired;

    host.invocation.marker("REPACK_WAYS_START");
    let way_stats = run_kind_phase(
        &shared_file,
        &way_schedule,
        ElementKind::Way,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    host.invocation.marker("REPACK_WAYS_END");
    emit_phase_counters(ElementKind::Way, &way_stats, &host.invocation);
    blobs_written += way_stats.blobs;
    elements_written += way_stats.elements;
    total_coalesces += way_stats.coalesces;
    total_worker_full_blobs += way_stats.worker_full_blobs;
    total_worker_tail_blobs += way_stats.worker_tail_blobs;
    total_central_blobs += way_stats.central_blobs;
    any_cap_fired |= way_stats.cap_fired;

    host.invocation.marker("REPACK_RELATIONS_START");
    let rel_stats = run_kind_phase(
        &shared_file,
        &rel_schedule,
        ElementKind::Relation,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    host.invocation.marker("REPACK_RELATIONS_END");
    emit_phase_counters(ElementKind::Relation, &rel_stats, &host.invocation);
    blobs_written += rel_stats.blobs;
    elements_written += rel_stats.elements;
    total_coalesces += rel_stats.coalesces;
    total_worker_full_blobs += rel_stats.worker_full_blobs;
    total_worker_tail_blobs += rel_stats.worker_tail_blobs;
    total_central_blobs += rel_stats.central_blobs;
    any_cap_fired |= rel_stats.cap_fired;

    host.invocation.marker("REPACK_FLUSH_START");
    writer.flush()?;
    host.invocation.marker("REPACK_FLUSH_END");

    host.invocation
        .counter("repack_blobs_written", blobs_written);
    host.invocation
        .counter("repack_elements_written", elements_written);
    host.invocation
        .counter("repack_input_blobs_coalesced", total_coalesces);
    host.invocation
        .counter("repack_worker_full_blobs", total_worker_full_blobs);
    host.invocation
        .counter("repack_worker_tail_blobs", total_worker_tail_blobs);
    host.invocation
        .counter("repack_central_blobs", total_central_blobs);
    host.invocation.counter("repack_cap_fired", any_cap_fired);
    host.invocation
        .counter("repack_elements_per_blob", elements_per_blob);

    // Detect the silent-identity surprise: cap exceeds every kind's total
    // element count, so no kind has ever flushed (in workers or in the
    // central builder) and the output is one blob per non-empty kind.
    // Only meaningful when there was real work to do.
    if !any_cap_fired && elements_written > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "repack.cap_never_fired",
            "the element cap of {elements_per_blob} per blob never fired; \
             every per-kind element count fits in a single output blob, \
             so the output is one blob per kind."
        );
    }

    Ok(RepackStats {
        blobs_written,
        elements_written,
        elements_per_blob,
    })
}

/// Per-kind phase totals returned to the top-level `repack` driver.
struct PhaseStats {
    blobs: u64,
    elements: u64,
    /// Count of grow blobs whose elements extended a non-empty central
    /// builder: 0 on a pure shrink, about one per grow blob on a pure grow.
    coalesces: u64,
    /// Blocks of `cap` elements framed in workers (shrink blobs).
    worker_full_blobs: u64,
    /// A shrink blob's remainder, framed in its worker as its own block.
    worker_tail_blobs: u64,
    /// Blocks of the merge thread's central builder (grow blobs): its cap
    /// fires, its flushes before a shrink blob's frames, and its final
    /// residual flush. The three counts sum to `blobs`.
    central_blobs: u64,
    /// True iff this kind produced more than one output blob, i.e. the
    /// cap actually shaped the output. Used by the global warning.
    cap_fired: bool,
}

/// Sidecar counter names for one kind's phase, in emission order: blobs,
/// elements, coalesces, worker_full_blobs, central_blobs, cap_fired,
/// worker_tail_blobs. Spelled out literally so each name can be grepped
/// verbatim.
fn phase_counter_names(kind: ElementKind) -> [&'static str; 7] {
    match kind {
        ElementKind::Node => [
            "repack_nodes_blobs",
            "repack_nodes_elements",
            "repack_nodes_coalesces",
            "repack_nodes_worker_full_blobs",
            "repack_nodes_central_blobs",
            "repack_nodes_cap_fired",
            "repack_nodes_worker_tail_blobs",
        ],
        ElementKind::Way => [
            "repack_ways_blobs",
            "repack_ways_elements",
            "repack_ways_coalesces",
            "repack_ways_worker_full_blobs",
            "repack_ways_central_blobs",
            "repack_ways_cap_fired",
            "repack_ways_worker_tail_blobs",
        ],
        ElementKind::Relation => [
            "repack_relations_blobs",
            "repack_relations_elements",
            "repack_relations_coalesces",
            "repack_relations_worker_full_blobs",
            "repack_relations_central_blobs",
            "repack_relations_cap_fired",
            "repack_relations_worker_tail_blobs",
        ],
    }
}

/// Emit per-kind sidecar counters for one `run_kind_phase` result.
fn emit_phase_counters(kind: ElementKind, s: &PhaseStats, invocation: &crate::host::Recorder) {
    let [
        blobs,
        elements,
        coalesces,
        worker_full,
        central,
        cap_fired,
        worker_tail,
    ] = phase_counter_names(kind);
    invocation.counter(blobs, s.blobs);
    invocation.counter(elements, s.elements);
    invocation.counter(coalesces, s.coalesces);
    invocation.counter(worker_full, s.worker_full_blobs);
    invocation.counter(central, s.central_blobs);
    invocation.counter(cap_fired, s.cap_fired);
    invocation.counter(worker_tail, s.worker_tail_blobs);
}

/// Run one per-kind phase: pread workers decode each blob and encode and
/// frame a shrink blob whole, or ship a grow blob's elements owned; the
/// merge thread writes the frames as they are and coalesces the grow blobs
/// in one `BlockBuilder` (see the module doc).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: ElementKind,
    cap: std::num::NonZeroUsize,
    prealloc_bound: usize,
    compression: Compression,
    preserve_locations: bool,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    host: &crate::host::Entered<'_>,
) -> Result<PhaseStats> {
    if schedule.is_empty() {
        return Ok(PhaseStats {
            blobs: 0,
            elements: 0,
            coalesces: 0,
            worker_full_blobs: 0,
            worker_tail_blobs: 0,
            central_blobs: 0,
            cap_fired: false,
        });
    }

    type PhaseResult = crate::Result<WorkerOutput>;

    // Same bound as the workers', derived from the input instead of a block:
    // the file cannot deliver more elements than it has bytes (an element
    // needs at least one byte on the wire). Undershooting only costs a vector
    // growth that in practice never happens; not bounding it at all let a
    // single CLI integer request an unbounded allocation.
    let mut bb = BlockBuilder::with_element_cap_prealloc(cap, prealloc_bound);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let cap_u64 = cap.get() as u64;
    let mut elements: u64 = 0;
    let mut coalesces: u64 = 0;
    let mut worker_full_blobs: u64 = 0;
    let mut worker_tail_blobs: u64 = 0;
    let mut central_blobs: u64 = 0;

    // Results arrive in schedule order through the ordered pool, whose window
    // bounds the decoded payloads in flight. A write or encode error ends the
    // phase at its position.
    crate::scan::classify::parallel_classify_phase_ordered(
        shared_file,
        schedule,
        None,
        None,
        // Each worker keeps one full-block builder and its scratch across
        // blobs. Its preallocation is the default block's at most: the cap
        // is user input, and the vectors grow if a block needs more.
        || WorkerScratch {
            builder: crate::commands::WorkerBuilder::new(BlockBuilder::with_element_cap_prealloc(
                cap,
                prealloc_bound.min(crate::tunables::DEFAULT_ELEMENT_CAP.get()),
            )),
            refs: Vec::new(),
            locations: Vec::new(),
        },
        |block, scratch| -> PhaseResult {
            worker_split_blob(
                block,
                kind,
                cap,
                preserve_locations,
                scratch,
                crate::commands::FramedBlocks::new(compression, &host.invocation.metrics().writer),
            )
        },
        |_position, item| {
            match item {
                WorkerOutput::Framed {
                    frames,
                    elements: n,
                } => {
                    // Ordering guard: this shrink blob's IDs are above every
                    // ID the central builder holds (the input is type-then-ID
                    // sorted and those came from earlier blobs), so the
                    // builder is flushed and its blocks handed to the writer
                    // before these frames. Raw frames and writer-framed
                    // blocks take their sequence numbers in submission order,
                    // so the output is ordered as submitted.
                    if !bb.is_empty() {
                        flush_local(&mut bb, &mut output)?;
                        central_blobs += write_central(&mut output, writer)?;
                    }
                    worker_full_blobs += n / cap_u64;
                    worker_tail_blobs += u64::from(n % cap_u64 != 0);
                    for frame in frames {
                        writer.write_raw_owned(frame)?;
                    }
                    elements += n;
                }
                WorkerOutput::Owned(tail) => {
                    let tail_n = tail.len();
                    if tail_n > 0 && !bb.is_empty() {
                        coalesces += 1;
                    }
                    match tail {
                        KindPayload::Nodes(nodes) => {
                            for node in &nodes {
                                write_single_node_local(node, &mut bb, &mut output)?;
                            }
                        }
                        KindPayload::Ways(ways) => {
                            for way in &ways {
                                write_single_way_local(way, &mut bb, &mut output)?;
                            }
                        }
                        KindPayload::Relations(rels) => {
                            for rel in &rels {
                                write_single_relation_local(rel, &mut bb, &mut output)?;
                            }
                        }
                    }
                    // Blocks the builder completed at its cap.
                    central_blobs += write_central(&mut output, writer)?;
                    elements += tail_n;
                }
            }
            Ok(())
        },
        host,
    )?;

    // Final flush: the residual block (if any) for this kind.
    flush_local(&mut bb, &mut output)?;
    central_blobs += write_central(&mut output, writer)?;

    let blobs = worker_full_blobs + worker_tail_blobs + central_blobs;
    Ok(PhaseStats {
        blobs,
        elements,
        coalesces,
        worker_full_blobs,
        worker_tail_blobs,
        central_blobs,
        cap_fired: blobs > 1,
    })
}

/// Hand every block in `output` to the pipelined writer, which frames it on
/// its executor (with its index and tagdata) and writes it in submission
/// order; returns how many there were.
fn write_central(
    output: &mut Vec<OwnedBlock>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
) -> Result<u64> {
    let n = output.len() as u64;
    for block in output.drain(..) {
        writer.write_primitive_block_owned(block)?;
    }
    Ok(n)
}

/// Encode one way into the per-worker full-block `BlockBuilder`, reusing the
/// caller's scratch buffers. When `preserve_locations` is set (input header
/// declares LocationsOnWays) the way's own inline coordinates are embedded via
/// `add_way_with_locations`. The length guard keeps a way that carries no
/// inline coordinates under a LOW header (empty lat/lon fields) on the plain
/// `add_way` path instead of tripping the refs==locations invariant.
fn encode_way_full_block(
    bb: &mut BlockBuilder,
    w: &crate::Way<'_>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
    preserve_locations: bool,
) -> crate::Result<()> {
    refs_buf.clear();
    refs_buf.extend(w.refs());
    let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
    if preserve_locations {
        locations_buf.clear();
        locations_buf.extend(
            w.node_locations()
                .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon())),
        );
    }
    if preserve_locations && locations_buf.len() == refs_buf.len() && !locations_buf.is_empty() {
        bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
    } else {
        bb.add_way(w.id(), w.tags(), refs_buf, meta.as_ref());
    }
    Ok(())
}

/// A repack worker's state across the blobs of one kind phase.
struct WorkerScratch {
    builder: crate::commands::WorkerBuilder,
    refs: Vec<i64>,
    locations: Vec<(i32, i32)>,
}

/// One worker's body: count the blob's elements of `kind` (`M`). With
/// `M >= cap` re-encode every one through the worker's `BlockBuilder`, each
/// block framed into `framed` as it fills, so the blob leaves as `M / cap`
/// full blocks and its `M % cap` remainder as its own block. With `M < cap`
/// own them for the merge thread's builder.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn worker_split_blob(
    block: &crate::PrimitiveBlock,
    kind: ElementKind,
    cap_nz: std::num::NonZeroUsize,
    preserve_locations: bool,
    scratch: &mut WorkerScratch,
    mut framed: crate::commands::FramedBlocks<'_>,
) -> crate::Result<WorkerOutput> {
    let cap = cap_nz.get();
    // Trust: the schedule put this blob in this kind's phase because its
    // indexdata names the kind, and only elements of that kind are counted and
    // re-encoded below. The schedule is built with `KindClaims::Refuse`, so
    // the pool that decoded this block has already refused it if it holds an
    // element of another kind than the blob claims (a mislabelled indexdata,
    // or a valid multi-kind block indexed by another writer's rule): nothing
    // is dropped silently. An unindexed blob (`--force`) is replicated into
    // every kind's phase with no claim stamped, so a no-match there is correct
    // and is not refused. See reference/pipeline.md "Blob Filtering".
    let is_match = |element: &Element<'_>| {
        matches!(
            (element, kind),
            (Element::DenseNode(_) | Element::Node(_), ElementKind::Node)
                | (Element::Way(_), ElementKind::Way)
                | (Element::Relation(_), ElementKind::Relation)
        )
    };
    // A block holding no other kind (every indexed blob, by the refusal
    // above) yields exactly its validated element count of this kind, so
    // only an unindexed blob pays a counting walk.
    let total = match usize::try_from(block.element_count()) {
        Ok(n) if block.kinds().foreign_to(kind).is_none() => n,
        _ => block.elements().filter(|e| is_match(e)).count(),
    };

    if total < cap {
        // A grow blob (or no match at all): its elements always go into the
        // merge thread's builder, so they leave owned.
        let mut tail: KindPayload = match kind {
            ElementKind::Node => KindPayload::Nodes(Vec::with_capacity(total)),
            ElementKind::Way => KindPayload::Ways(Vec::with_capacity(total)),
            ElementKind::Relation => KindPayload::Relations(Vec::with_capacity(total)),
        };
        for element in block.elements() {
            match (&element, &mut tail) {
                (Element::DenseNode(dn), KindPayload::Nodes(v)) => v.push(read_dense_node(dn)?),
                (Element::Node(n), KindPayload::Nodes(v)) => v.push(read_node(n)?),
                (Element::Way(w), KindPayload::Ways(v)) => v.push(if preserve_locations {
                    read_way_with_locations(w)?
                } else {
                    read_way(w)?
                }),
                (Element::Relation(r), KindPayload::Relations(v)) => v.push(read_relation(r)?),
                _ => {}
            }
        }
        return Ok(WorkerOutput::Owned(tail));
    }

    // A shrink blob: every element through the worker's builder, which
    // flushes at the cap (a block `ensure_*` flushes is framed into `framed`
    // on the spot); the final flush frames the remainder as its own block.
    let bb = scratch.builder.task()?;
    let refs_buf = &mut scratch.refs;
    let locations_buf = &mut scratch.locations;
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        match (&element, kind) {
            (Element::DenseNode(dn), ElementKind::Node) => {
                ensure_node_capacity_local(bb, &mut framed)?;
                let meta = dense_node_metadata(dn)?;
                bb.add_node(
                    dn.id(),
                    dn.decimicro_lat(),
                    dn.decimicro_lon(),
                    dn.tags(),
                    meta.as_ref(),
                );
            }
            (Element::Node(n), ElementKind::Node) => {
                ensure_node_capacity_local(bb, &mut framed)?;
                let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                bb.add_node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    n.tags(),
                    meta.as_ref(),
                );
            }
            (Element::Way(w), ElementKind::Way) => {
                ensure_way_capacity_local(bb, &mut framed)?;
                encode_way_full_block(bb, w, refs_buf, locations_buf, preserve_locations)?;
            }
            (Element::Relation(r), ElementKind::Relation) => {
                ensure_relation_capacity_local(bb, &mut framed)?;
                relation_members_into(r, &mut members_buf)?;
                let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
            }
            _ => {}
        }
    }
    flush_local(bb, &mut framed)?;
    crate::commands::release_scratch(&mut scratch.refs);
    crate::commands::release_scratch(&mut scratch.locations);
    let frames: Vec<Vec<u8>> = framed.frames.into_iter().map(|(_, frame)| frame).collect();

    Ok(WorkerOutput::Framed {
        frames,
        elements: total as u64,
    })
}

#[cfg(test)]
mod counter_name_tests {
    use super::*;

    #[test]
    fn phase_counter_names_match_the_old_format_spelling() {
        for (kind, word) in [
            (ElementKind::Node, "nodes"),
            (ElementKind::Way, "ways"),
            (ElementKind::Relation, "relations"),
        ] {
            let want = [
                "blobs",
                "elements",
                "coalesces",
                "worker_full_blobs",
                "central_blobs",
                "cap_fired",
                "worker_tail_blobs",
            ]
            .map(|c| format!("repack_{word}_{c}"));
            assert_eq!(phase_counter_names(kind).map(String::from), want);
        }
    }
}
