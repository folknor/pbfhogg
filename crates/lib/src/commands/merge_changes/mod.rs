//! Merge multiple OSC files into a single OSC stream.
//!
//! Default mode preserves the full change stream in input order.
//! `--simplify` keeps only the last change per object (type + id).

pub(crate) mod emit;
mod simplify;
pub(crate) mod stream;

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::btree_map::Entry;
use std::io::Write;
use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
#[cfg(test)]
use rayon::prelude::*;

use super::Result;
#[cfg(test)]
use crate::ElementKind;
#[cfg(test)]
use crate::osc::version_floor::VersionFloor;
#[cfg(test)]
use crate::osc::write::{
    OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay, write_node_xml,
    write_relation_xml, write_way_xml,
};
use crate::osc::xml_parse::OscAction;
#[cfg(test)]
use crate::osc::xml_parse::{
    DeleteChildren, OscSink, coord_to_decimicro, delete_coords, parse_i64_attr, parse_member,
    parse_nd, parse_osc_path, parse_owned_metadata, parse_tag,
};

/// `merge-changes` speaks the same three actions as the shared parser.
type Action = OscAction;

#[cfg(test)]
#[derive(Clone, Debug)]
enum ChangeElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

#[cfg(test)]
impl ChangeElement {
    fn key(&self) -> (ElementKind, i64) {
        match self {
            Self::Node(n) => (ElementKind::Node, n.id),
            Self::Way(w) => (ElementKind::Way, w.id),
            Self::Relation(r) => (ElementKind::Relation, r.id),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct Change {
    action: Action,
    element: ChangeElement,
}

#[cfg(test)]
#[derive(Default)]
struct ChangeStream {
    changes: Vec<Change>,
}

#[cfg(test)]
impl ChangeStream {
    fn push(&mut self, action: Action, element: ChangeElement) {
        self.changes.push(Change { action, element });
    }
}

#[derive(Debug, Default)]
pub struct MergeChangesStats {
    pub files: usize,
    pub changes_in: u64,
    pub changes_out: u64,
    pub simplified: bool,
}

impl MergeChangesStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        if self.simplified {
            writeln!(
                w,
                "Merged {} files: {} input changes -> {} output changes (simplified)",
                self.files, self.changes_in, self.changes_out
            )?;
        } else {
            writeln!(
                w,
                "Merged {} files: {} changes",
                self.files, self.changes_out
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
struct CurrentElem {
    kind: ElementKind,
    id: i64,
    location: Option<(i32, i32)>,
    metadata: Option<OwnedMetadata>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
}

#[cfg(test)]
impl CurrentElem {
    fn new(kind: ElementKind, id: i64, metadata: Option<OwnedMetadata>) -> Self {
        Self {
            kind,
            id,
            location: None,
            metadata,
            tags: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        }
    }

    fn into_change_element(self) -> ChangeElement {
        match self.kind {
            ElementKind::Node => ChangeElement::Node(OwnedNode {
                id: self.id,
                location: self.location,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElementKind::Way => ChangeElement::Way(OwnedWay {
                id: self.id,
                refs: self.refs,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElementKind::Relation => ChangeElement::Relation(OwnedRelation {
                id: self.id,
                members: self.members,
                tags: self.tags,
                metadata: self.metadata,
            }),
        }
    }
}

/// How [`merge_changes`] combines its inputs.
pub enum MergeMode {
    /// Every change, in input order, nothing resolved. Output produced for
    /// a later input while an earlier one is still being written is held in
    /// `scratch` until that input becomes the head. It is required when
    /// [`stream_writes_scratch`] says the run may spill, and refused as
    /// missing before any work; otherwise `None` is accepted, and a scratch
    /// given anyway is removed unused.
    Stream {
        scratch: Option<crate::scratch::RunScratch>,
    },
    /// Keep only the last change per object (type + id), by input position.
    /// A change whose version is lower than one already seen for the object
    /// is refused unless `allow_version_regression`. Writes no scratch.
    Simplify { allow_version_regression: bool },
}

/// Whether a [`MergeMode::Stream`] merge of `input_count` inputs at
/// `parallelism` may write scratch, so needs a [`RunScratch`]. One input
/// never does: its output goes straight to the destination. Neither does
/// `Parallelism::Fixed(1)`: inputs are then read one at a time, each only
/// once the one before it is written, so nothing is produced ahead of the
/// head. `Auto` may, whatever it resolves to on this host.
///
/// [`RunScratch`]: crate::scratch::RunScratch
#[must_use]
pub const fn stream_writes_scratch(
    input_count: usize,
    parallelism: crate::host::Parallelism,
) -> bool {
    input_count > 1 && !matches!(parallelism.fixed(), Some(1))
}

pub fn merge_changes(
    inputs: &[&Path],
    output: impl crate::output::BindOutput,
    mode: MergeMode,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
) -> Result<MergeChangesStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = merge_changes_to(inputs, &output, &mode, parallelism, host)?;
    // The run scratch goes before publication, as on every scratch path.
    drop(mode);
    output.publish()?;
    Ok(result)
}

/// [`merge_changes`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn merge_changes_to(
    inputs: &[&Path],
    output: &crate::output::BoundOutput,
    mode: &MergeMode,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Entered<'_>,
) -> Result<MergeChangesStats> {
    // Gzip or plain XML is decided by the name the caller chose (never the
    // staging name), and a name that is neither is refused before any work.
    let is_gz = crate::osc::output_is_gzip(output.destination())?;
    if inputs.is_empty() {
        return Err(crate::Error::invalid_argument(
            "at least one input OSC file is required",
        ));
    }
    if let MergeMode::Stream { scratch: None } = mode
        && stream_writes_scratch(inputs.len(), parallelism)
    {
        return Err(crate::Error::invalid_argument(
            "a merge of several inputs without simplify needs a scratch directory, unless it \
             runs on one worker",
        ));
    }
    let simplify = matches!(mode, MergeMode::Simplify { .. });

    let worker_count = parallelism.resolve(|| {
        crate::host::sized_workers(
            &*host.resources,
            crate::tunables::MERGE_CHANGES_WORKERS_RESERVED,
            crate::tunables::MERGE_CHANGES_WORKERS_MIN,
            crate::tunables::MERGE_CHANGES_WORKERS_FALLBACK,
        )
    });

    host.invocation.marker("MERGECHANGES_START");
    host.invocation.counter("merge_changes_files", inputs.len());
    host.invocation
        .counter("merge_changes_worker_count", worker_count);
    let total_bytes_in: u64 = inputs
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    host.invocation
        .counter("merge_changes_total_bytes_in", total_bytes_in);

    // Build a pool of the call's own when the caller fixed the parallelism.
    // Pool controls par_iter and par_chunks thread count for both the
    // streaming and simplify paths. Under `Auto` the host's executor is used
    // (rayon's ambient pool under `Executor::Ambient`).
    // The streaming path's ordered window spawns onto the same executor its
    // body is installed on, so the explicit pool is lent as one. It is joined
    // when `fixed_pool` drops: below on success, on any early return or
    // unwind otherwise.
    let fixed_pool = match parallelism.fixed() {
        Some(_) => Some(crate::joined_pool::JoinedPool::build(
            "merge-changes worker pool",
            worker_count,
            &host.invocation,
        )?),
        None => None,
    };
    let executor = fixed_pool.as_ref().map_or_else(
        || host.executor.borrowed(),
        |pool| crate::host::ExecutorRef::Pool(pool.pool()),
    );

    // The one whole-body executor install in the library. Everywhere else
    // `install` wraps only a parallel computation, so that no `PbfWriter`
    // producer ends up on a pool worker; this body writes OSC XML and drives
    // no `PbfWriter`, so wrapping all of it puts no producer on a worker.
    // Under `Executor::Ambient` (the CLI) this is a direct call.
    let cancel = &host.invocation;
    let (changes_in, changes_out) = executor.install(|| -> crate::Result<(u64, u64)> {
        match mode {
            MergeMode::Simplify {
                allow_version_regression,
            } => simplify::run(
                &stream::Plan {
                    inputs,
                    is_gz,
                    readers: worker_count,
                    executor,
                    cancel,
                },
                output,
                worker_count,
                *allow_version_regression,
            ),
            MergeMode::Stream { scratch } => {
                let changes_out = write_streaming(
                    inputs,
                    output,
                    is_gz,
                    worker_count,
                    executor,
                    scratch.as_ref(),
                    cancel,
                )?;
                Ok((changes_out, changes_out))
            }
        }
    })?;
    // Inside the marked span, so `MERGECHANGES_END` comes after the pool's
    // workers have exited.
    drop(fixed_pool);

    host.invocation
        .counter("merge_changes_changes_out", changes_out);
    if let Ok(meta) = std::fs::metadata(output.path()) {
        host.invocation
            .counter("merge_changes_total_bytes_out", meta.len());
    }
    host.invocation.marker("MERGECHANGES_END");

    Ok(MergeChangesStats {
        files: inputs.len(),
        changes_in,
        changes_out,
        simplified: simplify,
    })
}

/// Build the simplify path's pre-dedupe `ChangeStream` from N inputs.
///
/// N <= 1: serial, same as the pre-parallel shape. N > 1: rayon
/// `par_iter` fan-out into per-input streams, concatenated in input order
/// so `write_simplified`'s "later inputs win" `BTreeMap` dedupe still
/// observes inputs in sequence order.
#[cfg(test)]
fn build_simplify_stream(
    inputs: &[&Path],
    cancel: &crate::host::Recorder,
) -> crate::Result<ChangeStream> {
    if inputs.len() <= 1 {
        let mut stream = ChangeStream::default();
        for path in inputs {
            parse_one_into_stream(path, &mut stream, cancel)?;
        }
        return Ok(stream);
    }

    for path in inputs {
        if let Ok(meta) = std::fs::metadata(path) {
            cancel.counter("merge_changes_input_bytes", meta.len());
        }
    }

    cancel.marker("MERGECHANGES_PARALLEL_PARSE_START");
    let streams: crate::Result<Vec<ChangeStream>> = inputs
        .par_iter()
        .map(|path| -> crate::Result<ChangeStream> {
            let mut s = ChangeStream::default();
            parse_osc_into(path, &mut s, cancel)?;
            Ok(s)
        })
        .collect();
    let streams = streams?;
    cancel.marker("MERGECHANGES_PARALLEL_PARSE_END");

    let mut combined = ChangeStream::default();
    let total_changes: usize = streams.iter().map(|s| s.changes.len()).sum();
    combined.changes.reserve(total_changes);
    for s in streams {
        let len_before = combined.changes.len();
        combined.changes.extend(s.changes);
        cancel.counter(
            "merge_changes_changes_per_osc",
            combined.changes.len() - len_before,
        );
    }
    Ok(combined)
}

/// Wrap `parse_osc_into` in the `MERGECHANGES_PARSE_{START,END}` pair so
/// `brokkr sidecar --durations` can measure each input's parse wall, and
/// emit `merge_changes_changes_per_osc` on the post-parse stream length
/// delta so the per-input change count is observable in the sidecar.
#[cfg(test)]
fn parse_one_into_stream(
    path: &Path,
    stream: &mut ChangeStream,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        cancel.counter("merge_changes_input_bytes", meta.len());
    }
    let len_before = stream.changes.len();
    cancel.marker("MERGECHANGES_PARSE_START");
    let result = parse_osc_into(path, stream, cancel);
    cancel.marker("MERGECHANGES_PARSE_END");
    cancel.counter(
        "merge_changes_changes_per_osc",
        stream.changes.len() - len_before,
    );
    result
}

// ---------------------------------------------------------------------------
// The owned-element sink
//
// There used to be two copies of this state machine - `handle_start_like`
// and `handle_start_like_streaming` - identical apart from whether the
// finished element went into a `ChangeStream` or straight out through an
// `OscWriter`, and both differed from the compact overlay's machine in
// ways nobody intended. The framing now lives in
// `osc::xml_parse::drive_osc`; the destination is the only thing that
// varies, and it varies behind this one trait.
// ---------------------------------------------------------------------------

/// Where a finished change goes.
#[cfg(test)]
trait ChangeDest {
    /// Whether `accept` emits the change (serializes it toward the output),
    /// so that its failure is the output's, not a fault of the input; see
    /// `OscSink::END_ELEMENT_EMITS`.
    const EMITS: bool;

    fn accept(&mut self, change: Change) -> Result<()>;
}

#[cfg(test)]
struct StreamDest<'a> {
    stream: &'a mut ChangeStream,
}

#[cfg(test)]
impl ChangeDest for StreamDest<'_> {
    const EMITS: bool = false;

    fn accept(&mut self, change: Change) -> Result<()> {
        self.stream.push(change.action, change.element);
        Ok(())
    }
}

/// The owned path's writer: what [`emit::ByteSink`] replaced on the
/// streaming path, kept as the oracle its differential tests compare with.
#[cfg(test)]
struct WriterDest<'a> {
    writer: &'a mut OscWriter,
    open_action: &'a mut Option<Action>,
    count: &'a mut u64,
}

#[cfg(test)]
impl ChangeDest for WriterDest<'_> {
    const EMITS: bool = true;

    fn accept(&mut self, change: Change) -> Result<()> {
        emit_change(self.writer, self.open_action, &change)?;
        *self.count += 1;
        Ok(())
    }
}

#[cfg(test)]
struct OwnedSink<D: ChangeDest> {
    dest: D,
    action: Action,
    current: Option<CurrentElem>,
}

#[cfg(test)]
impl<D: ChangeDest> OwnedSink<D> {
    fn new(dest: D) -> Self {
        Self {
            dest,
            action: Action::Create,
            current: None,
        }
    }

    /// The element a child belongs to; every object, deleted ones included.
    fn current(&mut self) -> crate::Result<&mut CurrentElem> {
        self.current.as_mut().ok_or_else(|| {
            crate::Error::internal("merge-changes: an object child with no current object")
        })
    }
}

#[cfg(test)]
impl<D: ChangeDest> OscSink for OwnedSink<D> {
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;
    const END_ELEMENT_EMITS: bool = D::EMITS;

    fn begin_element(
        &mut self,
        action: Action,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> crate::Result<()> {
        let id = parse_i64_attr(e, "id")?;
        let metadata = parse_owned_metadata(e)?;
        let mut elem = CurrentElem::new(kind, id, metadata);

        // Coordinates are required of a created or modified node and
        // optional on a delete, kept when given. This used to be
        // `parse_f64_attr_optional(...).unwrap_or(0.0)` followed by a
        // saturating `as i32`, which put a coordinate-less node at 0/0 in
        // the Gulf of Guinea and clamped `lat="1e30"` to `i32::MAX`.
        // Same rule as the overlay parser now, because it is the same
        // function.
        if kind == ElementKind::Node {
            elem.location = if action == Action::Delete {
                delete_coords(e, id)?
            } else {
                Some((
                    coord_to_decimicro(e, "lat", id)?,
                    coord_to_decimicro(e, "lon", id)?,
                ))
            };
        }

        self.action = action;
        self.current = Some(elem);
        Ok(())
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        let tag = parse_tag(e)?;
        self.current()?.tags.push(tag);
        Ok(())
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        let rf = parse_nd(e)?;
        self.current()?.refs.push(rf);
        Ok(())
    }

    fn member(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        let member = parse_member(e)?;
        let id = member.member_id();
        self.current()?.members.push(OwnedMember {
            id,
            role: member.role,
        });
        Ok(())
    }

    fn end_element(&mut self) -> crate::Result<()> {
        let Some(elem) = self.current.take() else {
            return Ok(());
        };
        self.dest.accept(Change {
            action: self.action,
            element: elem.into_change_element(),
        })
    }
}

#[cfg(test)]
fn parse_osc_into(
    path: &Path,
    stream: &mut ChangeStream,
    cancel: &crate::host::Recorder,
) -> Result<()> {
    let mut sink = OwnedSink::new(StreamDest { stream });
    // Deliberately use the shared historical per-file decompression counter.
    // Command-specific names or accumulation would break brokkr comparisons.
    parse_osc_path(path, &mut sink, cancel)
}

/// An in-memory piece of OSC output: the XML is written to a plain buffer
/// and, for gzip, deflated in one call when the piece is finished. Writing
/// the events through a `GzEncoder` ran deflate once per tiny write, several
/// per element, which was most of a planet merge's worker time.
struct OscWriter {
    writer: Writer<Vec<u8>>,
    gz: bool,
}

impl OscWriter {
    fn write_event(&mut self, event: Event<'_>) -> Result<()> {
        self.writer.write_event(event)?;
        Ok(())
    }

    /// Bytes written so far (before any deflate).
    fn len(&self) -> usize {
        self.writer.get_ref().len()
    }

    /// The finished piece: the XML, as one self-contained gzip member when
    /// gzip was asked for.
    fn into_inner(self) -> Result<Vec<u8>> {
        let xml = self.writer.into_inner();
        if !self.gz {
            return Ok(xml);
        }
        let mut gz = GzEncoder::new(
            Vec::with_capacity(xml.len() / 4),
            flate2::Compression::fast(),
        );
        gz.write_all(&xml)?;
        Ok(gz.finish()?)
    }

    /// In-memory `OscWriter` for encoding workers. Each batch is emitted
    /// into its own `Vec<u8>` buffer (gz-encoded if `is_gz`); the ordered
    /// drain appends the buffers in input and batch order.
    /// Multi-member gzip is valid: the resulting output file is the
    /// concatenation of self-contained gzip streams, decoded as the
    /// concatenation of their decompressed contents.
    fn new_buf(is_gz: bool) -> Self {
        Self {
            writer: Writer::new_with_indent(Vec::new(), b' ', 2),
            gz: is_gz,
        }
    }
}

#[hotpath::measure]
fn write_streaming(
    inputs: &[&Path],
    output: &crate::output::BoundOutput,
    is_gz: bool,
    readers: usize,
    executor: crate::host::ExecutorRef<'_>,
    scratch: Option<&crate::scratch::RunScratch>,
    cancel: &crate::host::Recorder,
) -> Result<u64> {
    for path in inputs {
        if let Ok(meta) = std::fs::metadata(path) {
            cancel.counter("merge_changes_input_bytes", meta.len());
        }
    }
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let mut out = output.writer()?;
    out.write_all(&prelude)?;
    // The ordered drain overlaps this span; DRAIN below is the postlude.
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_START");
    let count = stream::run(
        &stream::Plan {
            inputs,
            is_gz,
            readers,
            executor,
            cancel,
        },
        scratch,
        &mut out,
    )?;
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_END");
    cancel.marker("MERGECHANGES_DRAIN_START");
    out.write_all(&postlude)?;
    out.finish()?;
    cancel.marker("MERGECHANGES_DRAIN_END");
    cancel.marker("MERGECHANGES_WRITE_FINISH_START");
    cancel.marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(count)
}

/// One batch's self-contained output member: replay the recorded callbacks
/// into the owned sink, re-emit XML and (under gzip) compress into an
/// in-memory buffer, opening and closing the batch's own action sections.
fn encode_batch(
    batch: &crate::osc::xml_parse::LexBatch,
    path: &Path,
    is_gz: bool,
) -> crate::Result<(Vec<u8>, u64)> {
    let mut writer = OscWriter::new_buf(is_gz);
    let mut open_action: Option<Action> = None;
    let mut count = 0u64;
    {
        // Checked as the owned sink checks, written as the owned writers
        // write, with no owned element (see `emit`).
        let mut sink = emit::ByteSink::new(&mut writer, &mut open_action, &mut count);
        batch.replay(&mut sink, path)?;
    }
    if let Some(prev) = open_action {
        writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
    }
    let bytes = writer.into_inner()?;
    Ok((bytes, count))
}

/// Pre-build the XML prelude (`<?xml ?><osmChange version="0.6">`) as
/// a self-contained gzip member (or plain bytes if `is_gz` is false).
/// Used by the N > 1 parallel-drain path so the main thread can write
/// it directly to the output file before concatenating worker chunks.
fn build_prelude_bytes(is_gz: bool) -> Result<Vec<u8>> {
    let mut writer = OscWriter::new_buf(is_gz);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;
    writer.into_inner()
}

/// Pre-build the XML postlude (`</osmChange>`) as a self-contained
/// gzip member (or plain bytes if `is_gz` is false). Counterpart to
/// `build_prelude_bytes`.
fn build_postlude_bytes(is_gz: bool) -> Result<Vec<u8>> {
    let mut writer = OscWriter::new_buf(is_gz);
    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    writer.into_inner()
}

#[cfg(test)]
fn emit_change(
    writer: &mut OscWriter,
    open_action: &mut Option<Action>,
    change: &Change,
) -> Result<()> {
    if *open_action != Some(change.action) {
        if let Some(prev) = open_action.take() {
            writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
        }
        writer.write_event(Event::Start(BytesStart::new(action_tag(change.action))))?;
        *open_action = Some(change.action);
    }
    write_change_to(writer, change)
}

/// The version an element claims, or `None` when it carried none.
#[cfg(test)]
fn change_version(change: &Change) -> Option<i32> {
    let meta = match &change.element {
        ChangeElement::Node(n) => n.metadata.as_ref(),
        ChangeElement::Way(w) => w.metadata.as_ref(),
        ChangeElement::Relation(r) => r.metadata.as_ref(),
    };
    meta.and_then(|m| m.version)
}

/// Refuse a `--simplify` dedupe in which the incoming change claims a
/// lower version than the object already had, then fold its claim into
/// the floor. An equal claim is admitted and wins by position, like any
/// admitted change; [`VersionFloor`] explains why.
///
/// `--simplify` keeps the last change per `(kind, id)` in **input**
/// order, so listing the inputs out of sequence order - which `brokkr`'s
/// `--osc-range` makes easy to do by accident - would silently regress
/// objects to older versions and report success. That is data
/// corruption, so it is refused by default.
///
/// The floor is the highest version ever claimed for the object, not
/// the version of the change it replaces: comparing only against the
/// previous change let an unversioned change in between reset it, so
/// `delete v9`, unversioned `modify`, `modify v8` was accepted here and
/// refused by apply-changes. [`VersionFloor`] holds the rule both use.
#[cfg(test)]
fn check_version_advance(floor: &mut VersionFloor, incoming: &Change) -> Result<()> {
    let claim = change_version(incoming);
    if let Some(version) = claim
        && let Err(held) = floor.admit(claim)
    {
        let (kind, id) = match &incoming.element {
            ChangeElement::Node(n) => (ElementKind::Node, n.id),
            ChangeElement::Way(w) => (ElementKind::Way, w.id),
            ChangeElement::Relation(r) => (ElementKind::Relation, r.id),
        };
        return Err(crate::error::new_version_regression(
            "merge-changes simplification",
            kind,
            id,
            version,
            held,
        ));
    }
    floor.raise(claim);
    Ok(())
}

#[cfg(test)]
fn write_simplified(
    output: &crate::output::BoundOutput,
    is_gz: bool,
    stream: ChangeStream,
    worker_count: usize,
    allow_version_regression: bool,
    cancel: &crate::host::Recorder,
) -> Result<usize> {
    cancel.check()?;
    cancel.marker("MERGECHANGES_SIMPLIFY_START");
    cancel.counter("merge_changes_changes_in", stream.changes.len());
    // Each object's last change, beside the highest version ever claimed
    // for it. The floor is only maintained while the gate is on; with
    // `--allow-version-regression` the dedupe is purely positional.
    // The owned simplify path, kept as the oracle `simplify::run` is held to
    // byte for byte (and regression for regression) by the tests below.
    let mut last_by_object: BTreeMap<(ElementKind, i64), (Change, VersionFloor)> = BTreeMap::new();
    for change in stream.changes {
        let key = change.element.key();
        if allow_version_regression {
            last_by_object.insert(key, (change, VersionFloor::default()));
            continue;
        }
        match last_by_object.entry(key) {
            Entry::Vacant(slot) => {
                let mut floor = VersionFloor::default();
                floor.raise(change_version(&change));
                slot.insert((change, floor));
            }
            Entry::Occupied(mut slot) => {
                let (last, floor) = slot.get_mut();
                check_version_advance(floor, &change)?;
                *last = change;
            }
        }
    }

    let mut creates = Vec::new();
    let mut modifies = Vec::new();
    let mut deletes = Vec::new();
    for (_, (change, _)) in last_by_object {
        match change.action {
            Action::Create => creates.push(change),
            Action::Modify => modifies.push(change),
            Action::Delete => deletes.push(change),
        }
    }
    cancel.marker("MERGECHANGES_SIMPLIFY_END");
    // Stage boundary: the in-memory dedupe above has no checkpoint of its
    // own (it is one pass over the parsed changes).
    cancel.check()?;

    let total_out = creates.len() + modifies.len() + deletes.len();
    // Mirror the streaming-path parallel-drain shape: split each
    // non-empty action group into chunks, parallel-emit each chunk as a
    // self-contained `<action>...</action>` gzip member, main thread
    // concatenates members in (group, chunk-index) order with the
    // shared prelude / postlude. Multiple consecutive same-action
    // sections are valid OSC; the per-chunk wrapping costs ~14 bytes
    // per chunk and is negligible against the win.
    //
    // 37fbe5b5 (parallel parse, serial write_simplified) clocked
    // planet 7-OSC --simplify at 220.9 s with the write_simplified
    // phase consuming ~197 s on a single thread - the same per-change
    // `quick_xml::Writer` + zlib ceiling that bottlenecked the
    // streaming path's abandoned parallel-parse-only stage. Same
    // mechanism, same fix.
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_START");
    let mut all_chunks: Vec<Vec<u8>> = Vec::new();
    for (action, group) in [
        (Action::Create, &creates),
        (Action::Modify, &modifies),
        (Action::Delete, &deletes),
    ] {
        if group.is_empty() {
            continue;
        }
        // One chunk per worker, each its own `<action>` section, so the
        // output bytes depend on the worker count (and through `-j 0` on
        // the host's core count) while the changes and their order do not.
        let chunk_len = group.len().div_ceil(worker_count).max(1);
        let chunks: crate::Result<Vec<Vec<u8>>> = group
            .par_chunks(chunk_len)
            .map(|chunk| -> crate::Result<Vec<u8>> {
                let mut writer = OscWriter::new_buf(is_gz);
                writer.write_event(Event::Start(BytesStart::new(action_tag(action))))?;
                for change in chunk {
                    write_change_to(&mut writer, change)?;
                }
                writer.write_event(Event::End(BytesEnd::new(action_tag(action))))?;
                writer.into_inner()
            })
            .collect();
        all_chunks.extend(chunks?);
        cancel.check()?;
    }
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_END");

    cancel.marker("MERGECHANGES_DRAIN_START");
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let mut out = output.writer()?;
    out.write_all(&prelude)?;
    for bytes in &all_chunks {
        out.write_all(bytes)?;
    }
    out.write_all(&postlude)?;
    out.finish()?;
    cancel.marker("MERGECHANGES_DRAIN_END");
    cancel.marker("MERGECHANGES_WRITE_FINISH_START");
    cancel.marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(total_out)
}

fn action_tag(action: Action) -> &'static str {
    action.tag()
}

/// A change's element, written the same way whatever its action: a delete
/// keeps the body its input gave.
#[cfg(test)]
fn write_change_to(writer: &mut OscWriter, change: &Change) -> Result<()> {
    write_change_element(&mut writer.writer, &change.element)
}

#[cfg(test)]
fn write_change_element<W: Write>(writer: &mut Writer<W>, element: &ChangeElement) -> Result<()> {
    match element {
        ChangeElement::Node(node) => write_node_xml(writer, node),
        ChangeElement::Way(way) => write_way_xml(writer, way),
        ChangeElement::Relation(rel) => write_relation_xml(writer, rel),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn gzip_members_preserve_document_framing() -> crate::Result<()> {
        use std::io::Read;
        let mut bytes = build_prelude_bytes(true)?;
        let mut body = OscWriter::new_buf(true);
        body.write_event(Event::Start(BytesStart::new("create")))?;
        body.write_event(Event::Empty(BytesStart::new("way")))?;
        body.write_event(Event::End(BytesEnd::new("create")))?;
        bytes.extend(body.into_inner()?);
        bytes.extend(build_postlude_bytes(true)?);
        let mut xml = String::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice()).read_to_string(&mut xml)?;
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<osmChange version=\"0.6\">"));
        assert!(xml.contains("<create>"));
        assert!(xml.ends_with("</osmChange>"));
        Ok(())
    }

    fn run_scratch(dir: &Path) -> crate::scratch::RunScratch {
        crate::scratch::RunScratch::create(dir, &crate::host::Host::entered_for_tests()).unwrap()
    }

    /// Every `id="N"` in document order.
    fn ids_in(xml: &str) -> Vec<i64> {
        xml.split(" id=\"")
            .skip(1)
            .map(|rest| rest[..rest.find('"').unwrap()].parse().unwrap())
            .collect()
    }

    // Inputs large enough to cut into several batches each, so later inputs
    // may spill and be copied back (whether they do is scheduling;
    // `a_forced_spill_is_copied_back_and_removed` forces it), plus an empty
    // input and action transitions
    // inside one input. The output must hold every change in input order,
    // and its bytes must not depend on the worker count: batch boundaries
    // come from the batch byte target alone. The one-thread pool exercises
    // the coordinator's cooperative wait.
    #[test]
    fn batched_merge_keeps_input_order_and_is_jobs_independent() -> crate::Result<()> {
        let scratch = crate::path_guard::tests::scratch();
        let per_input = crate::tunables::MERGE_CHANGES_BATCH_BYTES / 40;
        let mut paths = Vec::new();
        let mut expected_ids = Vec::new();
        for i in 0..5_i64 {
            let path = scratch.path().join(format!("input-{i}.osc"));
            let mut body = String::new();
            if i != 2 {
                body.push_str("<create>");
                for k in 0..i64::try_from(per_input)
                    .map_err(|e| crate::Error::internal(e.to_string()))?
                {
                    let id = i * 10_000_000 + k;
                    body.push_str(&format!(
                        "<node id=\"{id}\" lat=\"1\" lon=\"2\"><tag k=\"n\" v=\"{k}\"/></node>"
                    ));
                    expected_ids.push(id);
                }
                body.push_str("</create><delete>");
                let id = i * 10_000_000 + 9_999_999;
                body.push_str(&format!("<way id=\"{id}\"/></delete>"));
                expected_ids.push(id);
            }
            std::fs::write(
                &path,
                format!("<osmChange version=\"0.6\">{body}</osmChange>"),
            )?;
            paths.push(path);
        }
        let inputs: Vec<&Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        for is_gz in [false, true] {
            let mut first: Option<Vec<u8>> = None;
            for jobs in [1, 2, 8] {
                let name = if is_gz { "out.osc.gz" } else { "out.osc" };
                let output = scratch.path().join(name);
                let parallelism = crate::host::Parallelism::from_jobs(jobs);
                // One worker reads the inputs one after another, so it is
                // given no scratch: a spill there would be an error.
                let run_dir = stream_writes_scratch(inputs.len(), parallelism)
                    .then(|| run_scratch(scratch.path()));
                assert_eq!(run_dir.is_none(), jobs == 1);
                let stats = merge_changes(
                    &inputs,
                    crate::output::Output::new(
                        &output,
                        crate::output::Overwrite::Replace,
                        &crate::host::Diagnostics::discard(),
                    )?,
                    MergeMode::Stream { scratch: run_dir },
                    parallelism,
                    &crate::host::Host::entered_for_tests(),
                )?;
                assert_eq!(stats.changes_out, expected_ids.len() as u64);
                let bytes = std::fs::read(output)?;
                let xml = if is_gz {
                    use std::io::Read;
                    let mut xml = String::new();
                    flate2::read::MultiGzDecoder::new(bytes.as_slice()).read_to_string(&mut xml)?;
                    xml
                } else {
                    String::from_utf8(bytes.clone())
                        .map_err(|e| crate::Error::internal(e.to_string()))?
                };
                assert!(xml.ends_with("</osmChange>"), "jobs={jobs}, gzip={is_gz}");
                assert_eq!(ids_in(&xml), expected_ids, "jobs={jobs}, gzip={is_gz}");
                match &first {
                    None => first = Some(bytes),
                    Some(f) => assert_eq!(&bytes, f, "jobs={jobs}, gzip={is_gz}"),
                }
            }
        }
        Ok(())
    }

    /// Counter samples a run emits, by name.
    #[cfg(feature = "test-hooks")]
    #[derive(Default)]
    struct Counters(std::sync::Mutex<Vec<(String, i64)>>);

    #[cfg(feature = "test-hooks")]
    impl crate::debug::InstrumentSink for Counters {
        fn marker(&self, _: &str) {}
        fn counter(&self, name: &str, value: i64) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name.to_owned(), value));
        }
    }

    /// Input 1 forced to spill ahead of input 0 (the `test-hooks`
    /// spill-first handshake), once promoted with its spill file still open
    /// (it has more batches than are spilled first) and once with it closed
    /// (every batch spilled first). Each run writes the bytes the one-worker
    /// merge writes, reports the spilled bytes, takes the copy-back branch it
    /// was forced into, and has removed the spill file itself: the run
    /// directory, still alive after the merge, holds only its bookkeeping.
    #[cfg(feature = "test-hooks")]
    #[test]
    fn a_forced_spill_is_copied_back_and_removed() -> crate::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        let scratch = crate::path_guard::tests::scratch();
        // Several batches per input.
        let per_input = crate::tunables::MERGE_CHANGES_BATCH_BYTES / 20;
        let mut paths = Vec::new();
        for i in 0..2_usize {
            let path = scratch.path().join(format!("input-{i}.osc"));
            let mut body = String::from("<osmChange version=\"0.6\"><modify>");
            for k in 0..per_input {
                let id = i * 10_000_000 + k;
                body.push_str(&format!(
                    "<node id=\"{id}\" lat=\"1\" lon=\"2\"><tag k=\"n\" v=\"{k}\"/></node>"
                ));
            }
            body.push_str("</modify></osmChange>");
            std::fs::write(&path, body)?;
            paths.push(path);
        }
        let inputs: Vec<&Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        let reference = scratch.path().join("reference.osc");
        merge_changes(
            &inputs,
            crate::output::Output::new(
                &reference,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )?,
            MergeMode::Stream { scratch: None },
            crate::host::Parallelism::from_jobs(1),
            &crate::host::Host::entered_for_tests(),
        )?;
        let reference = std::fs::read(reference)?;

        // (members spilled first, promoted with the file open)
        for (first, open) in [(1, true), (1_000, false)] {
            let sink = std::sync::Arc::new(Counters::default());
            let host = crate::host::Host {
                invocation: crate::host::Invocation::with_sink(std::sync::Arc::clone(&sink) as _),
                ..crate::host::Host::for_tests()
            }
            .entered_leaked();
            let faults = host.invocation.faults();
            faults.merge_changes_spill_first.store(first, SeqCst);
            let dest = scratch.path().join(format!("out-{first}.osc"));
            let output = crate::output::bind_unissued(
                crate::output::Output::new(
                    &dest,
                    crate::output::Overwrite::Replace,
                    &crate::host::Diagnostics::discard(),
                )?,
                &host.invocation,
            )?;
            let mode = MergeMode::Stream {
                scratch: Some(run_scratch(scratch.path())),
            };
            merge_changes_to(
                &inputs,
                &output,
                &mode,
                crate::host::Parallelism::from_jobs(2),
                &host,
            )?;
            let case = format!("spill first {first}");
            assert!(!faults.merge_changes_hook_timed_out.load(SeqCst), "{case}");
            assert_eq!(
                faults.merge_changes_copied_open.load(SeqCst),
                u64::from(open),
                "{case}"
            );
            assert_eq!(
                faults.merge_changes_copied_closed.load(SeqCst),
                u64::from(!open),
                "{case}"
            );
            let MergeMode::Stream {
                scratch: Some(run_dir),
            } = &mode
            else {
                unreachable!()
            };
            let mut left = std::fs::read_dir(run_dir.path())?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<std::io::Result<Vec<_>>>()?;
            left.sort();
            assert_eq!(left, ["LOCK", "READY"], "{case}");
            drop(mode);
            output.publish()?;
            assert_eq!(std::fs::read(&dest)?, reference, "{case}");
            let spilled: Vec<i64> = sink
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|(name, _)| name == "merge_changes_spill_bytes")
                .map(|(_, value)| *value)
                .collect();
            assert_eq!(spilled.len(), 1, "{case}: {spilled:?}");
            assert!(spilled[0] > 0, "{case}: {spilled:?}");
        }
        Ok(())
    }

    /// Several inputs at any parallelism but one worker need a scratch
    /// directory, `Auto` included whatever it resolves to; the refusal comes
    /// before any input is read (the inputs here do not exist). One input,
    /// one worker and simplify need none.
    #[test]
    fn scratch_is_required_exactly_where_a_stream_merge_may_spill() {
        use crate::host::Parallelism;
        let scratch = crate::path_guard::tests::scratch();
        let missing = scratch.path().join("no-such-input.osc");
        let run = |inputs: &[&Path], mode: MergeMode, parallelism: Parallelism| {
            merge_changes(
                inputs,
                crate::output::Output::new(
                    scratch.path().join("out.osc"),
                    crate::output::Overwrite::Replace,
                    &crate::host::Diagnostics::discard(),
                )
                .unwrap(),
                mode,
                parallelism,
                &crate::host::Host::entered_for_tests(),
            )
            .unwrap_err()
            .to_string()
        };
        for parallelism in [Parallelism::Auto, Parallelism::from_jobs(2)] {
            let err = run(
                &[&missing, &missing],
                MergeMode::Stream { scratch: None },
                parallelism,
            );
            assert!(err.contains("scratch directory"), "{err}");
            assert!(!err.contains("no-such-input"), "{err}");
        }
        for (inputs, mode, parallelism) in [
            (
                vec![missing.as_path()],
                MergeMode::Stream { scratch: None },
                Parallelism::Auto,
            ),
            (
                vec![missing.as_path(), missing.as_path()],
                MergeMode::Stream { scratch: None },
                Parallelism::from_jobs(1),
            ),
            (
                vec![missing.as_path(), missing.as_path()],
                MergeMode::Simplify {
                    allow_version_regression: false,
                },
                Parallelism::Auto,
            ),
        ] {
            let err = run(&inputs, mode, parallelism);
            assert!(err.contains("no-such-input"), "reached the input: {err}");
        }
    }

    /// A scratch given to a merge that needs none is accepted and removed.
    #[test]
    fn an_unneeded_scratch_is_accepted_and_removed() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().join("work");
        std::fs::create_dir(&dir).unwrap();
        let input = dir.join("in.osc");
        std::fs::write(
            &input,
            "<osmChange version=\"0.6\"><create><node id=\"1\" lat=\"1\" lon=\"2\"/></create></osmChange>",
        )
        .unwrap();
        let output = dir.join("out.osc");
        for mode in [
            MergeMode::Stream {
                scratch: Some(run_scratch(&dir)),
            },
            MergeMode::Simplify {
                allow_version_regression: false,
            },
        ] {
            let stats = merge_changes(
                &[&input, &input],
                crate::output::Output::new(
                    &output,
                    crate::output::Overwrite::Replace,
                    &crate::host::Diagnostics::discard(),
                )
                .unwrap(),
                mode,
                crate::host::Parallelism::from_jobs(1),
                &crate::host::Host::entered_for_tests(),
            )
            .unwrap();
            assert_eq!(stats.files, 2);
            let mut names: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            assert_eq!(names, ["in.osc", "out.osc"]);
        }
    }

    /// The owned simplify path (the oracle): parse to owned changes, dedupe
    /// in a map, write the survivors.
    fn owned_simplify(
        inputs: &[&Path],
        output: &Path,
        jobs: usize,
        allow_version_regression: bool,
    ) -> crate::Result<Vec<u8>> {
        let host = crate::host::Host::entered_for_tests();
        let out = crate::output::bind_unissued(
            crate::output::Output::new(
                output,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )?,
            &host.invocation,
        )?;
        let is_gz = crate::osc::output_is_gzip(output)?;
        let stream = build_simplify_stream(inputs, &host.invocation)?;
        write_simplified(
            &out,
            is_gz,
            stream,
            jobs,
            allow_version_regression,
            &host.invocation,
        )?;
        out.publish()?;
        Ok(std::fs::read(output)?)
    }

    fn rendered_simplify(
        inputs: &[&Path],
        output: &Path,
        jobs: usize,
        allow_version_regression: bool,
    ) -> crate::Result<Vec<u8>> {
        merge_changes(
            inputs,
            crate::output::Output::new(
                output,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )?,
            MergeMode::Simplify {
                allow_version_regression,
            },
            crate::host::Parallelism::from_jobs(jobs),
            &crate::host::Host::entered_for_tests(),
        )?;
        Ok(std::fs::read(output)?)
    }

    /// Inputs that overlap on IDs across and within files, mix actions and
    /// kinds inside a section, carry metadata, escapes and unversioned
    /// changes, and are large enough to cut into several batches each.
    fn simplify_inputs(dir: &Path, regress: bool) -> Vec<std::path::PathBuf> {
        let per_input = i64::try_from(crate::tunables::MERGE_CHANGES_BATCH_BYTES / 60).unwrap();
        (0..4_i64)
            .map(|i| {
                let mut body = String::new();
                for k in 0..per_input {
                    let id = (k * 7 + i * 3) % (per_input / 2);
                    let version = if regress && i == 3 && k % 1000 == 17 {
                        1
                    } else {
                        i32::try_from(i + 2).unwrap()
                    };
                    let action = match (k + i) % 3 {
                        0 => "create",
                        1 => "modify",
                        _ => "delete",
                    };
                    let meta = if k % 5 == 0 {
                        String::new()
                    } else {
                        format!(" version=\"{version}\" user=\"u&amp;{k}\" uid=\"{k}\"")
                    };
                    let element = match k % 3 {
                        0 => format!(
                            "<node id=\"{id}\"{meta} lat=\"1.5\" lon=\"-2.25\"><tag k=\"n\" v=\"&lt;{k}\"/></node>"
                        ),
                        1 => format!(
                            "<way id=\"{id}\"{meta}><tag k=\"w\" v=\"{i}\"/><nd ref=\"{k}\"/><nd ref=\"1\"/></way>"
                        ),
                        _ => format!(
                            "<relation id=\"{}\"{meta}><member type=\"way\" ref=\"{k}\" role=\"r\"/></relation>",
                            -id
                        ),
                    };
                    body.push_str(&format!("<{action}>{element}</{action}>"));
                }
                let path = dir.join(format!("simplify-{i}.osc"));
                std::fs::write(&path, format!("<osmChange version=\"0.6\">{body}</osmChange>"))
                    .unwrap();
                path
            })
            .collect()
    }

    /// The rendered path writes exactly the owned path's bytes, gzip
    /// members included, at every worker count and with the gate off.
    #[test]
    fn rendered_simplify_writes_the_owned_bytes() -> crate::Result<()> {
        let scratch = crate::path_guard::tests::scratch();
        let paths = simplify_inputs(scratch.path(), false);
        let inputs: Vec<&Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        for name in ["out.osc", "out.osc.gz"] {
            let output = scratch.path().join(name);
            for jobs in [1, 3, 8] {
                for allow in [false, true] {
                    let owned = owned_simplify(&inputs, &output, jobs, allow)?;
                    let rendered = rendered_simplify(&inputs, &output, jobs, allow)?;
                    assert!(owned == rendered, "{name}, jobs={jobs}, allow={allow}");
                }
            }
        }
        Ok(())
    }

    /// With several objects regressing, both paths name the same one.
    #[test]
    fn rendered_simplify_reports_the_owned_regression() -> crate::Result<()> {
        let scratch = crate::path_guard::tests::scratch();
        let paths = simplify_inputs(scratch.path(), true);
        let inputs: Vec<&Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        let output = scratch.path().join("out.osc");
        let owned = owned_simplify(&inputs, &output, 2, false)
            .unwrap_err()
            .to_string();
        let rendered = rendered_simplify(&inputs, &output, 2, false)
            .unwrap_err()
            .to_string();
        assert!(owned.contains("version"), "{owned}");
        assert_eq!(owned, rendered);
        Ok(())
    }

    fn write_gz(path: &Path, xml: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = GzEncoder::new(file, flate2::Compression::fast());
        enc.write_all(xml.as_bytes()).unwrap();
        enc.finish().unwrap();
    }

    /// A truncated input - complete gzip member, no closing
    /// `</osmChange>` - is refused, and the destination the caller named
    /// must be untouched by the attempt.
    ///
    /// The single-input path streams XML into its output during the parse,
    /// so before publication moved into the library a failed merge called
    /// through the library replaced a pre-existing file with a partial
    /// document. The `Output` staging file is what keeps it out now.
    #[test]
    fn a_failed_merge_leaves_a_pre_existing_destination_intact() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let input = dir.join("cut.osc.gz");
        let dest = dir.join("out.osc.gz");
        write_gz(
            &input,
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n",
        );
        std::fs::write(&dest, b"precious user data").unwrap();

        let err = merge_changes(
            &[&input],
            crate::output::Output::new(
                &dest,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap(),
            MergeMode::Stream { scratch: None },
            crate::host::Parallelism::from_jobs(1),
            &crate::host::Host::entered_for_tests(),
        )
        .expect_err("a truncated input must be refused");
        assert!(
            err.to_string().contains("closing </osmChange>"),
            "error must name the missing closing tag; got: {err}"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"precious user data",
            "the destination must be byte-identical after a failed merge"
        );
    }

    /// The same failure against a destination that did not exist must
    /// not leave a partial file behind either - and must not leave the
    /// scratch sibling lying around.
    #[test]
    fn a_failed_merge_creates_no_partial_destination() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let input = dir.join("cut.osc.gz");
        let dest = dir.join("out.osc.gz");
        write_gz(
            &input,
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n",
        );

        merge_changes(
            &[&input],
            crate::output::Output::new(
                &dest,
                crate::output::Overwrite::Refuse,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap(),
            MergeMode::Stream { scratch: None },
            crate::host::Parallelism::from_jobs(1),
            &crate::host::Host::entered_for_tests(),
        )
        .expect_err("a truncated input must be refused");
        assert!(
            !dest.exists(),
            "a failed merge must not leave a partial destination"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n != "cut.osc.gz")
            .collect();
        assert!(leftovers.is_empty(), "scratch leaked: {leftovers:?}");
    }

    /// A successful merge still lands at the path the caller named, with
    /// the encoding the suffix implies - the counterpart that stops the
    /// two tests above from being satisfied by never writing anything.
    #[test]
    fn a_successful_merge_lands_at_the_named_path() {
        let scratch = crate::path_guard::tests::scratch();
        let dir = scratch.path().to_path_buf();
        let input = dir.join("in.osc.gz");
        let dest = dir.join("out.osc.gz");
        write_gz(
            &input,
            "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
             <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  \
             </create>\n</osmChange>",
        );
        std::fs::write(&dest, b"stale").unwrap();
        let stats = merge_changes(
            &[&input],
            crate::output::Output::new(
                &dest,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap(),
            MergeMode::Stream { scratch: None },
            crate::host::Parallelism::from_jobs(1),
            &crate::host::Host::entered_for_tests(),
        )
        .unwrap();
        assert_eq!(stats.changes_out, 1);
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(
            &bytes[..2],
            b"\x1f\x8b",
            "a .gz destination must still be gzip, so the scratch sibling kept the suffix"
        );
    }
}
