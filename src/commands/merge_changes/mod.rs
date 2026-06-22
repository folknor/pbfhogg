//! Merge multiple OSC files into a single OSC stream.
//!
//! Default mode preserves the full change stream in input order.
//! `--simplify` keeps only the last change per object (type + id).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Seek, Write};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::{Reader, Writer};
use rayon::prelude::*;

use super::Result;
use crate::MemberId;
use crate::osc::write::{
    OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay, write_delete_xml,
    write_node_xml, write_relation_xml, write_way_xml,
};

/// Wraps an `io::Read` and accumulates wall time spent in `read()` calls.
///
/// Used to attribute the gzip-decompress portion of `parse_osc_streaming`
/// and `parse_osc_into` separately from the surrounding `quick_xml` parse
/// machinery via the `merge_changes_decompress_ns` sidecar counter. The
/// shared `Rc<Cell<Duration>>` lets the wrapped reader live behind a
/// `Box<dyn io::Read>` while the outer parse function still recovers the
/// total at end-of-call.
struct TimedRead<R: io::Read> {
    inner: R,
    total: Rc<Cell<Duration>>,
}

impl<R: io::Read> io::Read for TimedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let start = Instant::now();
        let result = self.inner.read(buf);
        self.total.set(self.total.get() + start.elapsed());
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Create,
    Modify,
    Delete,
}

#[derive(Clone, Debug)]
enum ChangeElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

impl ChangeElement {
    fn key(&self) -> (u8, i64) {
        match self {
            Self::Node(n) => (0, n.id),
            Self::Way(w) => (1, w.id),
            Self::Relation(r) => (2, r.id),
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
    pub fn print_summary(&self) {
        if self.simplified {
            eprintln!(
                "Merged {} files: {} input changes -> {} output changes (simplified)",
                self.files, self.changes_in, self.changes_out
            );
        } else {
            eprintln!("Merged {} files: {} changes", self.files, self.changes_out);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

impl Section {
    fn as_action(self) -> Option<Action> {
        match self {
            Self::Create => Some(Action::Create),
            Self::Modify => Some(Action::Modify),
            Self::Delete => Some(Action::Delete),
            Self::None => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemKind {
    Node,
    Way,
    Relation,
}

struct CurrentElem {
    kind: ElemKind,
    id: i64,
    decimicro_lat: i32,
    decimicro_lon: i32,
    metadata: Option<OwnedMetadata>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
}

impl CurrentElem {
    fn new(kind: ElemKind, id: i64, metadata: Option<OwnedMetadata>) -> Self {
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
            ElemKind::Node => ChangeElement::Node(OwnedNode {
                id: self.id,
                decimicro_lat: self.decimicro_lat,
                decimicro_lon: self.decimicro_lon,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElemKind::Way => ChangeElement::Way(OwnedWay {
                id: self.id,
                refs: self.refs,
                tags: self.tags,
                metadata: self.metadata,
            }),
            ElemKind::Relation => ChangeElement::Relation(OwnedRelation {
                id: self.id,
                members: self.members,
                tags: self.tags,
                metadata: self.metadata,
            }),
        }
    }
}

#[hotpath::measure]
pub fn merge_changes(
    inputs: &[&Path],
    output: &Path,
    simplify: bool,
    jobs: Option<usize>,
) -> Result<MergeChangesStats> {
    if inputs.is_empty() {
        return Err("at least one input OSC file is required".into());
    }

    let worker_count = match jobs {
        Some(n) if n > 0 => n,
        _ => std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(8),
    };

    crate::debug::emit_marker("MERGECHANGES_START");
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_changes_files", inputs.len() as i64);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_changes_worker_count", worker_count as i64);
    let total_bytes_in: u64 = inputs
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_changes_total_bytes_in", total_bytes_in as i64);

    // Build a scoped rayon pool when the caller passed an explicit `jobs`.
    // Pool controls par_iter and par_chunks thread count for both the
    // streaming and simplify paths. When `jobs` is None, the global rayon
    // pool is used (current default behaviour).
    let pool = if jobs.is_some() {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker_count)
                .build()
                .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?,
        )
    } else {
        None
    };

    // The closure converts errors to String so it is Send across the
    // pool.install boundary; the project's `BoxResult` wraps a non-Send
    // `Box<dyn Error>` which `rayon::ThreadPool::install` cannot bridge.
    let do_work = || -> std::result::Result<(u64, u64), String> {
        if simplify {
            let stream = build_simplify_stream(inputs)?;
            let changes_in = stream.changes.len() as u64;
            let changes_out =
                write_simplified(output, stream, worker_count).map_err(|e| e.to_string())? as u64;
            Ok((changes_in, changes_out))
        } else {
            let changes_out = write_streaming(inputs, output).map_err(|e| e.to_string())?;
            Ok((changes_out, changes_out))
        }
    };

    let (changes_in, changes_out) = match &pool {
        Some(p) => p.install(do_work),
        None => do_work(),
    }
    .map_err(|s| -> Box<dyn std::error::Error> { s.into() })?;

    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_changes_changes_out", changes_out as i64);
    if let Ok(meta) = std::fs::metadata(output) {
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("merge_changes_total_bytes_out", meta.len() as i64);
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
/// observes inputs in sequence order. Errors are returned as `String`
/// so the closure can be `Send` across the `rayon::ThreadPool::install`
/// boundary in `merge_changes`.
fn build_simplify_stream(inputs: &[&Path]) -> std::result::Result<ChangeStream, String> {
    if inputs.len() <= 1 {
        let mut stream = ChangeStream::default();
        for path in inputs {
            parse_one_into_stream(path, &mut stream).map_err(|e| e.to_string())?;
        }
        return Ok(stream);
    }

    for path in inputs {
        if let Ok(meta) = std::fs::metadata(path) {
            #[allow(clippy::cast_possible_wrap)]
            crate::debug::emit_counter("merge_changes_input_bytes", meta.len() as i64);
        }
    }

    crate::debug::emit_marker("MERGECHANGES_PARALLEL_PARSE_START");
    let streams: std::result::Result<Vec<ChangeStream>, String> = inputs
        .par_iter()
        .map(|path| -> std::result::Result<ChangeStream, String> {
            let mut s = ChangeStream::default();
            parse_osc_into(path, &mut s).map_err(|e| e.to_string())?;
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
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter(
            "merge_changes_changes_per_osc",
            (combined.changes.len() - len_before) as i64,
        );
    }
    Ok(combined)
}

/// Wrap `parse_osc_into` in the `MERGECHANGES_PARSE_{START,END}` pair so
/// `brokkr sidecar --durations` can measure each input's parse wall, and
/// emit `merge_changes_changes_per_osc` on the post-parse stream length
/// delta so the per-input change count is observable in the sidecar.
fn parse_one_into_stream(path: &Path, stream: &mut ChangeStream) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("merge_changes_input_bytes", meta.len() as i64);
    }
    let len_before = stream.changes.len();
    crate::debug::emit_marker("MERGECHANGES_PARSE_START");
    let result = parse_osc_into(path, stream);
    crate::debug::emit_marker("MERGECHANGES_PARSE_END");
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter(
        "merge_changes_changes_per_osc",
        (stream.changes.len() - len_before) as i64,
    );
    result
}

#[hotpath::measure]
fn parse_osc_into(path: &Path, stream: &mut ChangeStream) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    io::Read::read_exact(&mut file, &mut magic)?;
    file.seek(io::SeekFrom::Start(0))?;

    let decompress_total = Rc::new(Cell::new(Duration::ZERO));
    let reader: Reader<BufReader<Box<dyn io::Read>>> = if magic == [0x1f, 0x8b] {
        let timed = TimedRead {
            inner: MultiGzDecoder::new(file),
            total: Rc::clone(&decompress_total),
        };
        Reader::from_reader(BufReader::new(Box::new(timed)))
    } else {
        let timed = TimedRead {
            inner: file,
            total: Rc::clone(&decompress_total),
        };
        Reader::from_reader(BufReader::new(Box::new(timed)))
    };
    let mut reader = reader;
    reader.config_mut().trim_text(true);

    let mut section = Section::None;
    let mut current: Option<CurrentElem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_like(e, false, &mut section, &mut current, stream)?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_start_like(e, true, &mut section, &mut current, stream)?;
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"create" | b"modify" | b"delete" => section = Section::None,
                b"node" | b"way" | b"relation" => {
                    finalize_current(section, &mut current, stream);
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    crate::debug::emit_counter(
        "merge_changes_decompress_ns",
        decompress_total.get().as_nanos() as i64,
    );
    Ok(())
}

fn handle_start_like(
    e: &BytesStart<'_>,
    is_empty: bool,
    section: &mut Section,
    current: &mut Option<CurrentElem>,
    stream: &mut ChangeStream,
) -> Result<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" | b"way" | b"relation" => {
            let kind = match e.name().as_ref() {
                b"node" => ElemKind::Node,
                b"way" => ElemKind::Way,
                _ => ElemKind::Relation,
            };
            let id = parse_i64_attr(e, b"id")?;
            let metadata = parse_metadata(e);
            let mut elem = CurrentElem::new(kind, id, metadata);

            if kind == ElemKind::Node {
                let lat = parse_f64_attr_optional(e, b"lat").unwrap_or(0.0);
                let lon = parse_f64_attr_optional(e, b"lon").unwrap_or(0.0);
                #[allow(clippy::cast_possible_truncation)]
                {
                    elem.decimicro_lat = (lat * 1e7).round() as i32;
                    elem.decimicro_lon = (lon * 1e7).round() as i32;
                }
            }

            if is_empty || *section == Section::Delete {
                let action = section
                    .as_action()
                    .ok_or_else(|| "element outside create/modify/delete section".to_string())?;
                stream.push(action, elem.into_change_element());
            } else {
                *current = Some(elem);
            }
        }
        b"tag" => {
            if let Some(cur) = current {
                let k = parse_str_attr(e, b"k")?;
                let v = parse_str_attr(e, b"v")?;
                cur.tags.push((k, v));
            }
        }
        b"nd" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Way
            {
                let rf = parse_i64_attr(e, b"ref")?;
                cur.refs.push(rf);
            }
        }
        b"member" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Relation
            {
                let ref_id = parse_i64_attr(e, b"ref")?;
                let role = parse_str_attr_optional(e, b"role").unwrap_or_default();
                let member_id = match parse_str_attr(e, b"type")?.as_str() {
                    "node" => MemberId::Node(ref_id),
                    "way" => MemberId::Way(ref_id),
                    "relation" => MemberId::Relation(ref_id),
                    other => {
                        return Err(format!("unknown relation member type '{other}'").into());
                    }
                };
                cur.members.push(OwnedMember {
                    id: member_id,
                    role,
                });
            }
        }
        _ => {}
    }

    if is_empty {
        match e.name().as_ref() {
            b"create" | b"modify" | b"delete" => *section = Section::None,
            _ => {}
        }
    }

    Ok(())
}

fn finalize_current(
    section: Section,
    current: &mut Option<CurrentElem>,
    stream: &mut ChangeStream,
) {
    let Some(elem) = current.take() else {
        return;
    };
    if let Some(action) = section.as_action() {
        stream.push(action, elem.into_change_element());
    }
}

fn parse_i64_attr(e: &BytesStart<'_>, name: &[u8]) -> Result<i64> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key == QName(name) {
            return Ok(std::str::from_utf8(&attr.value)?.parse::<i64>()?);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_metadata(e: &BytesStart<'_>) -> Option<OwnedMetadata> {
    let version = parse_i32_attr_optional(e, b"version")?;
    Some(OwnedMetadata {
        version,
        timestamp: parse_str_attr_optional(e, b"timestamp").unwrap_or_default(),
        changeset: parse_str_attr_optional(e, b"changeset").unwrap_or_default(),
        uid: parse_str_attr_optional(e, b"uid").unwrap_or_default(),
        user: parse_str_attr_optional(e, b"user").unwrap_or_default(),
        visible: parse_str_attr_optional(e, b"visible").unwrap_or_default(),
    })
}

fn parse_i32_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<i32> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            let text = std::str::from_utf8(&attr.value).ok()?;
            let parsed = text.parse::<i32>().ok()?;
            return Some(parsed);
        }
    }
    None
}

fn parse_f64_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<f64> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            let text = std::str::from_utf8(&attr.value).ok()?;
            let parsed = text.parse::<f64>().ok()?;
            return Some(parsed);
        }
    }
    None
}

fn parse_str_attr(e: &BytesStart<'_>, name: &[u8]) -> Result<String> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key == QName(name) {
            return Ok(attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                .into_owned());
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_str_attr_optional(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key == QName(name) {
            return attr
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(std::borrow::Cow::into_owned);
        }
    }
    None
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

impl OscWriter<io::BufWriter<File>> {
    fn from_file(output: &Path) -> Result<Self> {
        let file = File::create(output)?;
        let buf = io::BufWriter::new(file);
        let is_gz = output.to_str().is_some_and(|s| s.ends_with(".gz"));
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
        match self {
            Self::Gz(w) => {
                let gz = w.into_inner();
                gz.finish()?;
            }
            Self::Plain(w) => {
                let mut buf = w.into_inner();
                io::Write::flush(&mut buf)?;
            }
        }
        Ok(())
    }
}

impl OscWriter<Vec<u8>> {
    /// In-memory `OscWriter` for parallel-drain workers. Each worker
    /// emits its OSC's XML into its own `Vec<u8>` buffer (gz-encoded if
    /// `is_gz`); main thread concatenates worker buffers in input order.
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
fn write_streaming(inputs: &[&Path], output: &Path) -> Result<u64> {
    let is_gz = output.to_str().is_some_and(|s| s.ends_with(".gz"));

    if inputs.len() <= 1 {
        // N <= 1: no parallelism available. Use the original serial
        // streaming path that emits XML during parse - one-pass, no
        // buffer-and-drain cost. Avoids the regression a forced
        // buffer-and-drain would inflict at single-OSC scale (drain pass
        // is pure overhead when there is no parse to overlap with).
        crate::debug::emit_marker("MERGECHANGES_WRITE_OPEN_START");
        let mut writer = OscWriter::from_file(output)?;

        writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
        let mut root = BytesStart::new("osmChange");
        root.push_attribute(("version", "0.6"));
        writer.write_event(Event::Start(root))?;
        crate::debug::emit_marker("MERGECHANGES_WRITE_OPEN_END");

        let mut open_action: Option<Action> = None;
        let mut count = 0u64;
        for path in inputs {
            if let Ok(meta) = std::fs::metadata(path) {
                #[allow(clippy::cast_possible_wrap)]
                crate::debug::emit_counter("merge_changes_input_bytes", meta.len() as i64);
            }
            let count_before = count;
            crate::debug::emit_marker("MERGECHANGES_PARSE_START");
            parse_osc_streaming(path, &mut writer, &mut open_action, &mut count)?;
            crate::debug::emit_marker("MERGECHANGES_PARSE_END");
            #[allow(clippy::cast_possible_wrap)]
            crate::debug::emit_counter(
                "merge_changes_changes_per_osc",
                (count - count_before) as i64,
            );
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
    // Main thread writes the prelude (XML decl + osmChange opening) as its
    // own gzip member, concatenates worker buffers in input order (also
    // self-contained gzip members), and writes the postlude (osmChange
    // closing) as its own gzip member. Multi-member gzip is valid; on
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
            #[allow(clippy::cast_possible_wrap)]
            crate::debug::emit_counter("merge_changes_input_bytes", meta.len() as i64);
        }
    }

    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_START");
    let chunks: std::result::Result<Vec<(Vec<u8>, u64)>, String> = inputs
        .par_iter()
        .map(|path| -> std::result::Result<(Vec<u8>, u64), String> {
            let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
            let mut open_action: Option<Action> = None;
            let mut count = 0u64;
            parse_osc_streaming(path, &mut writer, &mut open_action, &mut count)
                .map_err(|e| e.to_string())?;
            if let Some(prev) = open_action {
                writer
                    .write_event(Event::End(BytesEnd::new(action_tag(prev))))
                    .map_err(|e| e.to_string())?;
            }
            let bytes = writer.into_inner().map_err(|e| e.to_string())?;
            Ok((bytes, count))
        })
        .collect();
    let chunks = chunks.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_END");

    crate::debug::emit_marker("MERGECHANGES_DRAIN_START");
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let file = File::create(output)?;
    let mut out = io::BufWriter::new(file);
    out.write_all(&prelude)?;
    let mut count = 0u64;
    for (bytes, n) in chunks {
        out.write_all(&bytes)?;
        count += n;
        #[allow(clippy::cast_possible_wrap)]
        crate::debug::emit_counter("merge_changes_changes_per_osc", n as i64);
    }
    out.write_all(&postlude)?;
    out.flush()?;
    crate::debug::emit_marker("MERGECHANGES_DRAIN_END");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_START");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(count)
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
) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    io::Read::read_exact(&mut file, &mut magic)?;
    file.seek(io::SeekFrom::Start(0))?;

    let decompress_total = Rc::new(Cell::new(Duration::ZERO));
    let reader: Reader<BufReader<Box<dyn io::Read>>> = if magic == [0x1f, 0x8b] {
        let timed = TimedRead {
            inner: MultiGzDecoder::new(file),
            total: Rc::clone(&decompress_total),
        };
        Reader::from_reader(BufReader::new(Box::new(timed)))
    } else {
        let timed = TimedRead {
            inner: file,
            total: Rc::clone(&decompress_total),
        };
        Reader::from_reader(BufReader::new(Box::new(timed)))
    };
    let mut reader = reader;
    reader.config_mut().trim_text(true);

    let mut section = Section::None;
    let mut current: Option<CurrentElem> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                handle_start_like_streaming(
                    e,
                    false,
                    &mut section,
                    &mut current,
                    writer,
                    open_action,
                    count,
                )?;
            }
            Ok(Event::Empty(ref e)) => {
                handle_start_like_streaming(
                    e,
                    true,
                    &mut section,
                    &mut current,
                    writer,
                    open_action,
                    count,
                )?;
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"create" | b"modify" | b"delete" => section = Section::None,
                b"node" | b"way" | b"relation" => {
                    if let Some(elem) = current.take()
                        && let Some(action) = section.as_action()
                    {
                        let change = Change {
                            action,
                            element: elem.into_change_element(),
                        };
                        emit_change(writer, open_action, &change)?;
                        *count += 1;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(Box::new(e)),
        }
        buf.clear();
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    crate::debug::emit_counter(
        "merge_changes_decompress_ns",
        decompress_total.get().as_nanos() as i64,
    );
    Ok(())
}

fn handle_start_like_streaming<W: io::Write>(
    e: &BytesStart<'_>,
    is_empty: bool,
    section: &mut Section,
    current: &mut Option<CurrentElem>,
    writer: &mut OscWriter<W>,
    open_action: &mut Option<Action>,
    count: &mut u64,
) -> Result<()> {
    match e.name().as_ref() {
        b"create" => *section = Section::Create,
        b"modify" => *section = Section::Modify,
        b"delete" => *section = Section::Delete,
        b"node" | b"way" | b"relation" => {
            let kind = match e.name().as_ref() {
                b"node" => ElemKind::Node,
                b"way" => ElemKind::Way,
                _ => ElemKind::Relation,
            };
            let id = parse_i64_attr(e, b"id")?;
            let metadata = parse_metadata(e);
            let mut elem = CurrentElem::new(kind, id, metadata);

            if kind == ElemKind::Node {
                let lat = parse_f64_attr_optional(e, b"lat").unwrap_or(0.0);
                let lon = parse_f64_attr_optional(e, b"lon").unwrap_or(0.0);
                #[allow(clippy::cast_possible_truncation)]
                {
                    elem.decimicro_lat = (lat * 1e7).round() as i32;
                    elem.decimicro_lon = (lon * 1e7).round() as i32;
                }
            }

            if is_empty || *section == Section::Delete {
                let action = section
                    .as_action()
                    .ok_or_else(|| "element outside create/modify/delete section".to_string())?;
                let change = Change {
                    action,
                    element: elem.into_change_element(),
                };
                emit_change(writer, open_action, &change)?;
                *count += 1;
            } else {
                *current = Some(elem);
            }
        }
        b"tag" => {
            if let Some(cur) = current {
                let k = parse_str_attr(e, b"k")?;
                let v = parse_str_attr(e, b"v")?;
                cur.tags.push((k, v));
            }
        }
        b"nd" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Way
            {
                let rf = parse_i64_attr(e, b"ref")?;
                cur.refs.push(rf);
            }
        }
        b"member" => {
            if let Some(cur) = current
                && cur.kind == ElemKind::Relation
            {
                let ref_id = parse_i64_attr(e, b"ref")?;
                let role = parse_str_attr_optional(e, b"role").unwrap_or_default();
                let member_id = match parse_str_attr(e, b"type")?.as_str() {
                    "node" => MemberId::Node(ref_id),
                    "way" => MemberId::Way(ref_id),
                    "relation" => MemberId::Relation(ref_id),
                    other => {
                        return Err(format!("unknown relation member type '{other}'").into());
                    }
                };
                cur.members.push(OwnedMember {
                    id: member_id,
                    role,
                });
            }
        }
        _ => {}
    }

    if is_empty {
        match e.name().as_ref() {
            b"create" | b"modify" | b"delete" => *section = Section::None,
            _ => {}
        }
    }

    Ok(())
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

#[hotpath::measure]
fn write_simplified(output: &Path, stream: ChangeStream, worker_count: usize) -> Result<usize> {
    crate::debug::emit_marker("MERGECHANGES_SIMPLIFY_START");
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("merge_changes_changes_in", stream.changes.len() as i64);
    let mut last_by_object: BTreeMap<(u8, i64), Change> = BTreeMap::new();
    for change in stream.changes {
        last_by_object.insert(change.element.key(), change);
    }

    let mut creates = Vec::new();
    let mut modifies = Vec::new();
    let mut deletes = Vec::new();
    for (_, change) in last_by_object {
        match change.action {
            Action::Create => creates.push(change),
            Action::Modify => modifies.push(change),
            Action::Delete => deletes.push(change),
        }
    }
    crate::debug::emit_marker("MERGECHANGES_SIMPLIFY_END");

    let total_out = creates.len() + modifies.len() + deletes.len();
    let is_gz = output.to_str().is_some_and(|s| s.ends_with(".gz"));

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
        let chunk_len = group.len().div_ceil(worker_count).max(1);
        let chunks: std::result::Result<Vec<Vec<u8>>, String> = group
            .par_chunks(chunk_len)
            .map(|chunk| -> std::result::Result<Vec<u8>, String> {
                let mut writer = OscWriter::<Vec<u8>>::new_buf(is_gz);
                writer
                    .write_event(Event::Start(BytesStart::new(action_tag(action))))
                    .map_err(|e| e.to_string())?;
                for change in chunk {
                    write_change_to(&mut writer, change).map_err(|e| e.to_string())?;
                }
                writer
                    .write_event(Event::End(BytesEnd::new(action_tag(action))))
                    .map_err(|e| e.to_string())?;
                writer.into_inner().map_err(|e| e.to_string())
            })
            .collect();
        let chunks = chunks.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        all_chunks.extend(chunks);
    }
    crate::debug::emit_marker("MERGECHANGES_PARALLEL_EMIT_END");

    crate::debug::emit_marker("MERGECHANGES_DRAIN_START");
    let prelude = build_prelude_bytes(is_gz)?;
    let postlude = build_postlude_bytes(is_gz)?;
    let file = File::create(output)?;
    let mut out = io::BufWriter::new(file);
    out.write_all(&prelude)?;
    for bytes in &all_chunks {
        out.write_all(bytes)?;
    }
    out.write_all(&postlude)?;
    out.flush()?;
    crate::debug::emit_marker("MERGECHANGES_DRAIN_END");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_START");
    crate::debug::emit_marker("MERGECHANGES_WRITE_FINISH_END");

    Ok(total_out)
}

fn action_tag(action: Action) -> &'static str {
    match action {
        Action::Create => "create",
        Action::Modify => "modify",
        Action::Delete => "delete",
    }
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
        let (tag, id, meta) = match element {
            ChangeElement::Node(n) => ("node", n.id, n.metadata.as_ref()),
            ChangeElement::Way(w) => ("way", w.id, w.metadata.as_ref()),
            ChangeElement::Relation(r) => ("relation", r.id, r.metadata.as_ref()),
        };
        write_delete_xml(writer, tag, id, meta)
    } else {
        match element {
            ChangeElement::Node(node) => write_node_xml(writer, node),
            ChangeElement::Way(way) => write_way_xml(writer, way),
            ChangeElement::Relation(rel) => write_relation_xml(writer, rel),
        }
    }
}
