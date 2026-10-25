//! Merge multiple OSC files into a single OSC stream.
//!
//! Default mode preserves the full change stream in input order.
//! `--simplify` keeps only the last change per object (type + id).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::io::{self, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use rayon::prelude::*;

use super::Result;
use crate::osc::version_floor::VersionFloor;
use crate::osc::write::{
    OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay, write_delete_xml,
    write_node_xml, write_relation_xml, write_way_xml,
};
use crate::osc::xml_parse::{
    OscAction, OscSink, coord_to_decimicro, parse_i64_attr, parse_member_type, parse_osc_path,
    parse_owned_metadata, parse_str_attr, parse_str_attr_optional,
};
use crate::{ElementKind, MemberId};

/// `merge-changes` speaks the same three actions as the shared parser.
type Action = OscAction;

#[derive(Clone, Debug)]
enum ChangeElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

impl ChangeElement {
    fn key(&self) -> (ElementKind, i64) {
        match self {
            Self::Node(n) => (ElementKind::Node, n.id),
            Self::Way(w) => (ElementKind::Way, w.id),
            Self::Relation(r) => (ElementKind::Relation, r.id),
        }
    }
}

#[derive(Clone, Debug)]
struct Change {
    action: Action,
    element: ChangeElement,
}

#[derive(Default)]
struct ChangeStream {
    changes: Vec<Change>,
}

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

struct CurrentElem {
    kind: ElementKind,
    id: i64,
    decimicro_lat: i32,
    decimicro_lon: i32,
    metadata: Option<OwnedMetadata>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
}

impl CurrentElem {
    fn new(kind: ElementKind, id: i64, metadata: Option<OwnedMetadata>) -> Self {
        Self {
            kind,
            id,
            decimicro_lat: 0,
            decimicro_lon: 0,
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
                decimicro_lat: self.decimicro_lat,
                decimicro_lon: self.decimicro_lon,
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

pub fn merge_changes(
    inputs: &[&Path],
    output: crate::output::Output,
    simplify: bool,
    parallelism: crate::host::Parallelism,
    allow_version_regression: bool,
    host: &crate::host::Host,
) -> Result<MergeChangesStats> {
    host.cancel.check()?;
    let result = merge_changes_to(
        inputs,
        &output,
        simplify,
        parallelism,
        allow_version_regression,
        host,
    )?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`merge_changes`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn merge_changes_to(
    inputs: &[&Path],
    output: &crate::output::Output,
    simplify: bool,
    parallelism: crate::host::Parallelism,
    allow_version_regression: bool,
    host: &crate::host::Host,
) -> Result<MergeChangesStats> {
    // Gzip or plain XML is decided by the name the caller chose (never the
    // staging name), and a name that is neither is refused before any work.
    let is_gz = crate::osc::output_is_gzip(output.destination())?;
    if inputs.is_empty() {
        return Err(crate::Error::invalid_argument(
            "at least one input OSC file is required",
        ));
    }

    let worker_count = parallelism.resolve(|| {
        crate::host::sized_workers(
            &*host.resources,
            crate::tunables::MERGE_CHANGES_WORKERS_RESERVED,
            crate::tunables::MERGE_CHANGES_WORKERS_MIN,
            crate::tunables::MERGE_CHANGES_WORKERS_FALLBACK,
        )
    });

    crate::debug::emit_marker("MERGECHANGES_START");
    crate::debug::emit_counter("merge_changes_files", inputs.len());
    crate::debug::emit_counter("merge_changes_worker_count", worker_count);
    let total_bytes_in: u64 = inputs
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    crate::debug::emit_counter("merge_changes_total_bytes_in", total_bytes_in);

    // Build a scoped rayon pool when the caller fixed the parallelism.
    // Pool controls par_iter and par_chunks thread count for both the
    // streaming and simplify paths. Under `Auto` the host's executor is used
    // (rayon's ambient pool under `Executor::Ambient`).
    // The streaming path's ordered window spawns onto the same executor its
    // body is installed on, so the explicit pool is carried as one.
    let executor = if parallelism.fixed().is_some() {
        crate::host::Executor::Pool(std::sync::Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker_count)
                .build()
                .map_err(|e| crate::Error::pool_build("merge-changes worker pool", e))?,
        ))
    } else {
        host.executor.clone()
    };

    // The one whole-body executor install in the library. Everywhere else
    // `install` wraps only a parallel computation, so that no `PbfWriter`
    // producer ends up on a pool worker; this body writes OSC XML and drives
    // no `PbfWriter`, so wrapping all of it puts no producer on a worker.
    // Under `Executor::Ambient` (the CLI) this is a direct call.
    let cancel = &host.cancel;
    let (changes_in, changes_out) = executor.install(|| -> crate::Result<(u64, u64)> {
        if simplify {
            let stream = build_simplify_stream(inputs, cancel)?;
            let changes_in = stream.changes.len() as u64;
            let changes_out = write_simplified(
                output,
                is_gz,
                stream,
                worker_count,
                allow_version_regression,
                cancel,
            )? as u64;
            Ok((changes_in, changes_out))
        } else {
            let changes_out = write_streaming(inputs, output, is_gz, &executor, cancel)?;
            Ok((changes_out, changes_out))
        }
    })?;

    crate::debug::emit_counter("merge_changes_changes_out", changes_out);
    if let Ok(meta) = std::fs::metadata(output.path()) {
        crate::debug::emit_counter("merge_changes_total_bytes_out", meta.len());
    }
    crate::debug::emit_marker("MERGECHANGES_END");

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
fn build_simplify_stream(
    inputs: &[&Path],
    cancel: &crate::host::Cancel,
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
            crate::debug::emit_counter("merge_changes_input_bytes", meta.len());
        }
    }

    crate::debug::emit_marker("MERGECHANGES_PARALLEL_PARSE_START");
    let streams: crate::Result<Vec<ChangeStream>> = inputs
        .par_iter()
        .map(|path| -> crate::Result<ChangeStream> {
            let mut s = ChangeStream::default();
            parse_osc_into(path, &mut s, cancel)?;
            Ok(s)
        })
        .collect();
    let streams = streams?;
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_PARSE_END");

    let mut combined = ChangeStream::default();
    let total_changes: usize = streams.iter().map(|s| s.changes.len()).sum();
    combined.changes.reserve(total_changes);
    for s in streams {
        let len_before = combined.changes.len();
        combined.changes.extend(s.changes);
        crate::debug::emit_counter(
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
fn parse_one_into_stream(
    path: &Path,
    stream: &mut ChangeStream,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        crate::debug::emit_counter("merge_changes_input_bytes", meta.len());
    }
    let len_before = stream.changes.len();
    crate::debug::emit_marker("MERGECHANGES_PARSE_START");
    let result = parse_osc_into(path, stream, cancel);
    crate::debug::emit_marker("MERGECHANGES_PARSE_END");
    crate::debug::emit_counter(
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
trait ChangeDest {
    /// Whether `accept` emits the change (serializes it toward the output),
    /// so that its failure is the output's, not a fault of the input; see
    /// `OscSink::END_ELEMENT_EMITS`.
    const EMITS: bool;

    fn accept(&mut self, change: Change) -> Result<()>;
}

struct StreamDest<'a> {
    stream: &'a mut ChangeStream,
}

impl ChangeDest for StreamDest<'_> {
    const EMITS: bool = false;

    fn accept(&mut self, change: Change) -> Result<()> {
        self.stream.push(change.action, change.element);
        Ok(())
    }
}

struct WriterDest<'a, W: io::Write> {
    writer: &'a mut OscWriter<W>,
    open_action: &'a mut Option<Action>,
    count: &'a mut u64,
}

impl<W: io::Write> ChangeDest for WriterDest<'_, W> {
    const EMITS: bool = true;

    fn accept(&mut self, change: Change) -> Result<()> {
        emit_change(self.writer, self.open_action, &change)?;
        *self.count += 1;
        Ok(())
    }
}

struct OwnedSink<D: ChangeDest> {
    dest: D,
    action: Action,
    current: Option<CurrentElem>,
}

impl<D: ChangeDest> OwnedSink<D> {
    fn new(dest: D) -> Self {
        Self {
            dest,
            action: Action::Create,
            current: None,
        }
    }
}

impl<D: ChangeDest> OscSink for OwnedSink<D> {
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
        // meaningless on a delete. This used to be
        // `parse_f64_attr_optional(...).unwrap_or(0.0)` followed by a
        // saturating `as i32`, which put a coordinate-less node at 0/0 in
        // the Gulf of Guinea and clamped `lat="1e30"` to `i32::MAX`.
        // Same rule as the overlay parser now, because it is the same
        // function.
        if kind == ElementKind::Node && action != Action::Delete {
            elem.decimicro_lat = coord_to_decimicro(e, "lat", id)?;
            elem.decimicro_lon = coord_to_decimicro(e, "lon", id)?;
        }

        self.action = action;
        self.current = Some(elem);
        Ok(())
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        if let Some(cur) = self.current.as_mut() {
            let k = parse_str_attr(e, "k")?;
            let v = parse_str_attr(e, "v")?;
            cur.tags.push((k, v));
        }
        Ok(())
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        let rf = parse_i64_attr(e, "ref")?;
        if let Some(cur) = self.current.as_mut() {
            cur.refs.push(rf);
        }
        Ok(())
    }

    fn member(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        let ref_id = parse_i64_attr(e, "ref")?;
        // Deliberately normalize readable roles and replace malformed roles with
        // empty text. Rejecting them would break the accepted OSC input contract.
        let role = parse_str_attr_optional(e, "role").unwrap_or_default();
        let member_id = match parse_member_type(&parse_str_attr(e, "type")?)? {
            crate::MemberType::Node => MemberId::Node(ref_id),
            crate::MemberType::Way => MemberId::Way(ref_id),
            crate::MemberType::Relation => MemberId::Relation(ref_id),
            crate::MemberType::Unknown(v) => {
                return Err(crate::Error::invalid_data(format!(
                    "unknown relation member type value '{v}'"
                )));
            }
        };
        if let Some(cur) = self.current.as_mut() {
            cur.members.push(OwnedMember {
                id: member_id,
                role,
            });
        }
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

#[hotpath::measure]
fn parse_osc_into(
    path: &Path,
    stream: &mut ChangeStream,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let mut sink = OwnedSink::new(StreamDest { stream });
    // Deliberately use the shared historical per-file decompression counter.
    // Command-specific names or accumulation would break brokkr comparisons.
    parse_osc_path(path, &mut sink, cancel)
}

enum OscWriter<W: io::Write> {
    Gz(Box<Writer<GzEncoder<W>>>),
    Plain(Writer<W>),
}

impl<W: io::Write> OscWriter<W> {
    fn write_event(&mut self, event: Event<'_>) -> Result<()> {
        match self {
            Self::Gz(w) => w.write_event(event)?,
            Self::Plain(w) => w.write_event(event)?,
        }
        Ok(())
    }

    /// Closes the gzip stream (if Gz) and returns the inner writer.
    /// For the file-backed flow, prefer `finish()` which also flushes
    /// the BufWriter. For the in-memory worker flow, this is the way
    /// to recover the produced `Vec<u8>` of compressed (or plain) XML.
    fn into_inner(self) -> Result<W> {
        match self {
            Self::Gz(w) => {
                let gz = w.into_inner();
                Ok(gz.finish()?)
            }
            Self::Plain(w) => Ok(w.into_inner()),
        }
    }
}

impl OscWriter<crate::output::OutputWriter> {
    fn from_output(
        output: &crate::output::Output,
        is_gz: bool,
        cancel: &crate::host::Cancel,
    ) -> Result<Self> {
        let buf = output.writer(cancel)?;
        if is_gz {
            Ok(Self::Gz(Box::new(Writer::new_with_indent(
                GzEncoder::new(buf, flate2::Compression::fast()),
                b' ',
                2,
            ))))
        } else {
            Ok(Self::Plain(Writer::new_with_indent(buf, b' ', 2)))
        }
    }

    fn finish(self) -> Result<()> {
        let buf = match self {
            Self::Gz(w) => w.into_inner().finish()?,
            Self::Plain(w) => w.into_inner(),
        };
        buf.finish()
    }
}

impl OscWriter<Vec<u8>> {
    /// In-memory `OscWriter` for parallel-drain workers. Each worker
    /// emits its OSC's XML into its own `Vec<u8>` buffer (gz-encoded if
    /// `is_gz`); the ordered drain appends worker buffers in input order.
    /// Multi-member gzip is valid: the resulting output file is the
    /// concatenation of self-contained gzip streams, decoded as the
    /// concatenation of their decompressed contents.
    fn new_buf(is_gz: bool) -> Self {
        if is_gz {
            Self::Gz(Box::new(Writer::new_with_indent(
                GzEncoder::new(Vec::new(), flate2::Compression::fast()),
                b' ',
                2,
            )))
        } else {
            Self::Plain(Writer::new_with_indent(Vec::new(), b' ', 2))
        }
    }
}

#[hotpath::measure]
fn write_streaming(
    inputs: &[&Path],
    output: &crate::output::Output,
    is_gz: bool,
    executor: &crate::host::Executor,
    cancel: &crate::host::Cancel,
) -> Result<u64> {
    crate::debug::emit_counter(
        "merge_changes_buffered_strategy",
        i64::from(inputs.len() > 1),
    );
    if inputs.len() <= 1 {
        // N <= 1: no parallelism available. Use the original serial
        // streaming path that emits XML during parse - one-pass, no
        // buffer-and-drain cost. Avoids the regression a forced
        // buffer-and-drain would inflict at single-OSC scale (drain pass
        // is pure overhead when there is no parse to overlap with).
        crate::debug::emit_marker("MERGECHANGES_WRITE_OPEN_START");
        let mut writer = OscWriter::from_output(output, is_gz, cancel)?;

        writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
        let mut root = BytesStart::new("osmChange");
        root.push_attribute(("version", "0.6"));
        writer.write_event(Event::Start(root))?;
        crate::debug::emit_marker("MERGECHANGES_WRITE_OPEN_END");

        let mut open_action: Option<Action> = None;
        let mut count = 0u64;
        for path in inputs {
            if let Ok(meta) = std::fs::metadata(path) {
                crate::debug::emit_counter("merge_changes_input_bytes", meta.len());
            }
            let count_before = count;
            crate::debug::emit_marker("MERGECHANGES_PARSE_START");
            parse_osc_streaming(path, &mut writer, &mut open_action, &mut count, cancel)?;
            crate::debug::emit_marker("MERGECHANGES_PARSE_END");
            crate::debug::emit_counter("merge_changes_changes_per_osc", count - count_before);
        }

        if let Some(prev) = open_action {
            writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
        }
        writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
        crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_START");
        writer.finish()?;
        crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_END");
        return Ok(count);
    }

    // N > 1: parallel-drain. Each worker runs the full per-input pipeline
    // (parse + XML re-emit + gzip-compress) into its own in-memory buffer.
    // The prelude (XML decl + osmChange opening) is written as its own gzip
    // member, the ordered drain appends worker buffers in input order (also
    // self-contained gzip members), and the postlude (osmChange closing) is
    // written as its own gzip member. Multi-member gzip is valid; on
    // decompress the members concatenate to produce the full XML document.
    //
    // Why this beats parallel-parse + serial-drain (commit 43dd620,
    // UUID 07ee92ee, 235.8 s wall): the serial drain pass at planet
    // 7-OSC was 223 s, dominated by per-change `quick_xml::Writer`
    // emit cost (single-thread XML serialization of 26.3 M changes).
    // Moving the re-emit + gzip onto the worker threads parallelizes
    // that 223 s across the same N rayon workers already doing parse.
    for path in inputs {
        if let Ok(meta) = std::fs::metadata(path) {
            crate::debug::emit_counter("merge_changes_input_bytes", meta.len());
        }
    }

    // Bounded ordered window: at most one member per pool worker is alive
    // between emit and drain, and the head member is written as soon as it
    // and its predecessors are done, so workers never idle behind a batch
    // barrier and memory does not grow with the input count. One huge input
    // remains unbounded; this is a member-count bound, not a byte budget.
    // Each input keeps its own writer and member boundary, so output bytes
    // are identical to collecting every member first.
    let window = rayon::current_num_threads().max(1);
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let mut out = output.writer(cancel)?;
    out.write_all(&prelude)?;
    let mut count = 0u64;
    // The per-member drain overlaps this span; DRAIN below is the postlude.
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_START");
    emit_members_ordered(executor, inputs, is_gz, window, cancel, |bytes, n| {
        out.write_all(&bytes)?;
        count += n;
        crate::debug::emit_counter("merge_changes_changes_per_osc", n);
        Ok(())
    })?;
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_END");
    crate::debug::emit_marker("MERGECHANGES_DRAIN_START");
    out.write_all(&postlude)?;
    out.finish()?;
    crate::debug::emit_marker("MERGECHANGES_DRAIN_END");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_START");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(count)
}

/// One input's self-contained output member: parse, XML re-emit and (under
/// gzip) compression into an in-memory buffer, closing any open action.
fn emit_member(
    path: &Path,
    is_gz: bool,
    cancel: &crate::host::Cancel,
) -> crate::Result<(Vec<u8>, u64)> {
    let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
    let mut open_action: Option<Action> = None;
    let mut count = 0u64;
    parse_osc_streaming(path, &mut writer, &mut open_action, &mut count, cancel)?;
    if let Some(prev) = open_action {
        writer.write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
    }
    let bytes = writer.into_inner()?;
    Ok((bytes, count))
}

type MemberResult = (usize, crate::Result<(Vec<u8>, u64)>);

/// Emit every input's member on `executor`'s pool and hand them to `drain`
/// in input order, with at most `window` members alive at once.
///
/// The same admission-credit shape as `scan::classify`'s ordered classify
/// phase: input `k` is spawned only once `k < drained + window`, so a slow
/// head input stalls admission rather than letting finished members pile
/// up behind it, and the in-flight positions stay distinct modulo the
/// ring. The drain runs on a pool worker and waits cooperatively, running
/// queued members itself, so a one-thread pool cannot deadlock. Returns the
/// first error received.
///
/// Cancellation: each member's parse stops at its next input read, and the
/// drain checks the token after every member it receives. Every spawned
/// member still sends its (cancelled) result, so no admitted position goes
/// unanswered; the scope then waits only for members already running.
fn emit_members_ordered(
    executor: &crate::host::Executor,
    inputs: &[&Path],
    is_gz: bool,
    window: usize,
    cancel: &crate::host::Cancel,
    mut drain: impl FnMut(Vec<u8>, u64) -> crate::Result<()> + Send,
) -> crate::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let window = window.clamp(1, inputs.len());
    let (tx, rx) = std::sync::mpsc::channel::<MemberResult>();
    // `move`: the body may run on a pool worker, and a borrowed Receiver
    // is not Send.
    executor.scope(move |scope| {
        let spawn = |position: usize| {
            let tx = tx.clone();
            let path = inputs[position];
            scope.spawn(move |_| {
                // A panicking member must still answer its position, or the
                // drain would wait for it forever.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    emit_member(path, is_gz, cancel)
                }))
                .unwrap_or_else(|payload| {
                    Err(
                        crate::error::panic_error("merge-changes worker", payload.as_ref())
                            .context(format!("while merging {}", path.display())),
                    )
                });
                drop(tx.send((position, result)));
            });
        };
        let mut pending: Vec<Option<(Vec<u8>, u64)>> = (0..window).map(|_| None).collect();
        let mut dispatched = 0;
        while dispatched < window {
            spawn(dispatched);
            dispatched += 1;
        }
        let mut next = 0;
        while next < inputs.len() {
            let (position, result) = recv_member(&rx, executor)?;
            cancel.check()?;
            pending[position % window] = Some(result?);
            while let Some((bytes, n)) = pending[next % window].take() {
                drain(bytes, n)?;
                next += 1;
                if dispatched < inputs.len() {
                    spawn(dispatched);
                    dispatched += 1;
                }
            }
        }
        Ok(())
    })
}

/// Receive one member, running queued pool jobs while waiting when the
/// caller is a pool worker (the writer's `recv_permit` shape).
fn recv_member(
    rx: &std::sync::mpsc::Receiver<MemberResult>,
    executor: &crate::host::Executor,
) -> crate::Result<MemberResult> {
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    let lost = || crate::Error::internal("merge-changes workers exited without a result");
    loop {
        match rx.try_recv() {
            Ok(item) => return Ok(item),
            Err(TryRecvError::Disconnected) => return Err(lost()),
            Err(TryRecvError::Empty) => match executor.yield_now() {
                Some(rayon::Yield::Executed) => {}
                Some(rayon::Yield::Idle) => {
                    match rx.recv_timeout(crate::tunables::EXECUTOR_IDLE_RECV_TIMEOUT) {
                        Ok(item) => return Ok(item),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Err(lost()),
                    }
                }
                None => return rx.recv().map_err(|_| lost()),
            },
        }
    }
}

/// Pre-build the XML prelude (`<?xml ?><osmChange version="0.6">`) as
/// a self-contained gzip member (or plain bytes if `is_gz` is false).
/// Used by the N > 1 parallel-drain path so the main thread can write
/// it directly to the output file before concatenating worker chunks.
fn build_prelude_bytes(is_gz: bool) -> Result<Vec<u8>> {
    let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
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
    let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    writer.into_inner()
}

#[hotpath::measure]
fn parse_osc_streaming<W: io::Write>(
    path: &Path,
    writer: &mut OscWriter<W>,
    open_action: &mut Option<Action>,
    count: &mut u64,
    cancel: &crate::host::Cancel,
) -> Result<()> {
    let mut sink = OwnedSink::new(WriterDest {
        writer,
        open_action,
        count,
    });
    // Deliberately use the shared historical per-file decompression counter.
    // Command-specific names or accumulation would break brokkr comparisons.
    parse_osc_path(path, &mut sink, cancel)
}

fn emit_change<W: io::Write>(
    writer: &mut OscWriter<W>,
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

#[hotpath::measure]
fn write_simplified(
    output: &crate::output::Output,
    is_gz: bool,
    stream: ChangeStream,
    worker_count: usize,
    allow_version_regression: bool,
    cancel: &crate::host::Cancel,
) -> Result<usize> {
    cancel.check()?;
    crate::debug::emit_marker("MERGECHANGES_SIMPLIFY_START");
    crate::debug::emit_counter("merge_changes_changes_in", stream.changes.len());
    // Each object's last change, beside the highest version ever claimed
    // for it. The floor is only maintained while the gate is on; with
    // `--allow-version-regression` the dedupe is purely positional.
    // Memory is O(input changes + surviving payload), not three copies of the
    // payload: this loop moves changes out of the stream, then moves survivors
    // out of the map into action groups. The stream's outer allocation remains
    // until the loop ends. A position index still retains every overwritten
    // payload until compaction, and must preserve version-floor checks and the
    // (kind, ID) order that defines group chunking and gzip member bytes. Keep
    // this measured path until a replacement demonstrates lower peak RSS and
    // equivalent bytes; disk spooling would add another full payload write/read.
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
    crate::debug::emit_marker("MERGECHANGES_SIMPLIFY_END");
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
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_START");
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
                let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
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
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_END");

    crate::debug::emit_marker("MERGECHANGES_DRAIN_START");
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let mut out = output.writer(cancel)?;
    out.write_all(&prelude)?;
    for bytes in &all_chunks {
        out.write_all(bytes)?;
    }
    out.write_all(&postlude)?;
    out.finish()?;
    crate::debug::emit_marker("MERGECHANGES_DRAIN_END");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_START");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(total_out)
}

fn action_tag(action: Action) -> &'static str {
    action.tag()
}

fn write_change_to<W: io::Write>(writer: &mut OscWriter<W>, change: &Change) -> Result<()> {
    let delete = change.action == Action::Delete;
    match writer {
        OscWriter::Gz(w) => write_change_element(w, &change.element, delete),
        OscWriter::Plain(w) => write_change_element(w, &change.element, delete),
    }
}

fn write_change_element<W: Write>(
    writer: &mut Writer<W>,
    element: &ChangeElement,
    delete: bool,
) -> Result<()> {
    if delete {
        let (kind, id, meta) = match element {
            ChangeElement::Node(n) => (ElementKind::Node, n.id, n.metadata.as_ref()),
            ChangeElement::Way(w) => (ElementKind::Way, w.id, w.metadata.as_ref()),
            ChangeElement::Relation(r) => (ElementKind::Relation, r.id, r.metadata.as_ref()),
        };
        write_delete_xml(writer, kind, id, meta)
    } else {
        match element {
            ChangeElement::Node(node) => write_node_xml(writer, node),
            ChangeElement::Way(way) => write_way_xml(writer, way),
            ChangeElement::Relation(rel) => write_relation_xml(writer, rel),
        }
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

    // Reconstruct the previous all-results-at-once byte sequence independently
    // of admission order, including empty inputs and action transitions. The
    // one-thread pool exercises the drain's cooperative wait (window 1).
    #[test]
    fn ordered_window_preserves_buffered_member_bytes() -> crate::Result<()> {
        let scratch = crate::path_guard::tests::scratch();
        let mut paths = Vec::new();
        for i in 0..5 {
            let path = scratch.path().join(format!("input-{i}.osc"));
            let body = if i == 2 {
                String::new()
            } else {
                format!(
                    "<create><node id=\"{}\" lat=\"1\" lon=\"2\"/></create>\
                     <delete><way id=\"{}\"/></delete>",
                    i + 1,
                    i + 10,
                )
            };
            std::fs::write(
                &path,
                format!("<osmChange version=\"0.6\">{body}</osmChange>"),
            )?;
            paths.push(path);
        }
        let inputs: Vec<&Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        for is_gz in [false, true] {
            let mut expected = build_prelude_bytes(is_gz)?;
            for path in &inputs {
                let mut writer = OscWriter::new_buf(is_gz);
                let mut action = None;
                let mut count = 0;
                parse_osc_streaming(
                    path,
                    &mut writer,
                    &mut action,
                    &mut count,
                    &crate::host::Cancel::new(),
                )?;
                if let Some(action) = action {
                    writer.write_event(Event::End(BytesEnd::new(action_tag(action))))?;
                }
                expected.extend(writer.into_inner()?);
            }
            expected.extend(build_postlude_bytes(is_gz)?);
            for jobs in [1, 2, 8] {
                let name = if is_gz { "out.osc.gz" } else { "out.osc" };
                let output = scratch.path().join(name);
                merge_changes(
                    &inputs,
                    crate::output::Output::new(
                        &output,
                        crate::output::Overwrite::Replace,
                        &crate::host::Diagnostics::discard(),
                    )?,
                    false,
                    crate::host::Parallelism::from_jobs(jobs),
                    false,
                    &crate::host::Host::for_tests(),
                )?;
                assert_eq!(
                    std::fs::read(output)?,
                    expected,
                    "jobs={jobs}, gzip={is_gz}"
                );
            }
        }
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
            false,
            crate::host::Parallelism::from_jobs(1),
            false,
            &crate::host::Host::for_tests(),
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
            false,
            crate::host::Parallelism::from_jobs(1),
            false,
            &crate::host::Host::for_tests(),
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
            false,
            crate::host::Parallelism::from_jobs(1),
            false,
            &crate::host::Host::for_tests(),
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
