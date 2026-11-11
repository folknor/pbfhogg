//! Filter OSC files by tag expressions while preserving all delete actions.

use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use crate::ElementKind;
use crate::commands::merge_changes::emit::ElementBuf;
use crate::commands::merge_changes::stream;
use crate::error::Result;
#[cfg(test)]
use crate::osc::write::{
    OwnedMember, OwnedNode, OwnedRelation, OwnedWay, write_node_xml, write_relation_xml,
    write_way_xml,
};
use crate::osc::xml_parse::{DeleteChildren, LexBatch, OscAction, OscSink};
#[cfg(test)]
use crate::osc::xml_parse::{
    coord_to_decimicro, delete_coords, parse_i64_attr, parse_member, parse_nd, parse_osc_path,
    parse_owned_metadata, parse_tag,
};
use crate::tag_expr::{Expression, parse_expressions, tag_matches};

// The owned path below (`matches_any` through `write_filtered_osc`) is what
// the byte path replaced, kept as the oracle its differential test compares
// with.

// This tag-first traversal borrows owned OSC tags without building a
// temporary borrowed-tag vector.
// It selects an entire create/modify element; deletes bypass this predicate.
// An empty expression list matches nothing, unlike export's optional filter.
// Single-tag wildcard and negation semantics remain owned by tag_matches.
#[cfg(test)]
fn matches_any(expressions: &[Expression], tags: &[(String, String)], kind: ElementKind) -> bool {
    for (key, value) in tags {
        for expr in expressions {
            if expr.type_filter.admits(kind) && tag_matches(&expr.matcher, key, value) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// OSC model used by this command
// ---------------------------------------------------------------------------

/// This command speaks the shared parser's vocabulary. It used to carry
/// its own `Section` and `ElementKind` around a third hand-rolled copy of
/// the OSC state machine - one the change-stream bug hunt did not even
/// find, and which had drifted the same way the other two had: node
/// coordinates defaulting to 0/0 in the Gulf of Guinea, a
/// `MemberId::Unknown` arm the other parsers refuse, no truncation
/// check, and an unconditional `MultiGzDecoder` that made
/// `tags-filter --input-kind osc` unable to read a plain `.osc`.
type Section = OscAction;

#[cfg(test)]
struct CurrentElem {
    kind: ElementKind,
    id: i64,
    location: Option<(i32, i32)>,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    members: Vec<OwnedMember>,
    /// The element's `version` / `timestamp` / `changeset` / `uid` /
    /// `user` / `visible`, or `None` when it carried none.
    ///
    /// This field did not exist: every element this filter kept was
    /// re-emitted with a hardcoded `metadata: None`, so running a change
    /// stream through `tags-filter --input-kind osc` silently erased the
    /// metadata from every surviving change. It is the same data loss
    /// that `merge-changes` was fixed for, in the sink the hunt found
    /// last, and it is worse here because a filter is expected to drop
    /// only what does not match.
    metadata: Option<crate::osc::write::OwnedMetadata>,
}

#[cfg(test)]
impl CurrentElem {
    fn new(kind: ElementKind, id: i64, metadata: Option<crate::osc::write::OwnedMetadata>) -> Self {
        Self {
            kind,
            id,
            location: None,
            tags: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
            metadata,
        }
    }
}

// Nine action/kind vectors: create/modify/delete then node/way/relation
// order, encounter order within each group. The byte path keeps the same
// nine groups as rendered XML.
#[cfg(test)]
struct FilteredOsc {
    create_nodes: Vec<OwnedNode>,
    create_ways: Vec<OwnedWay>,
    create_relations: Vec<OwnedRelation>,
    modify_nodes: Vec<OwnedNode>,
    modify_ways: Vec<OwnedWay>,
    modify_relations: Vec<OwnedRelation>,
    delete_nodes: Vec<OwnedNode>,
    delete_ways: Vec<OwnedWay>,
    delete_relations: Vec<OwnedRelation>,
}

#[cfg(test)]
impl FilteredOsc {
    fn new() -> Self {
        Self {
            create_nodes: Vec::new(),
            create_ways: Vec::new(),
            create_relations: Vec::new(),
            modify_nodes: Vec::new(),
            modify_ways: Vec::new(),
            modify_relations: Vec::new(),
            delete_nodes: Vec::new(),
            delete_ways: Vec::new(),
            delete_relations: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct TagsFilterOscStats {
    pub creates_in: u64,
    pub creates_out: u64,
    pub modifies_in: u64,
    pub modifies_out: u64,
    pub deletes_in: u64,
    pub deletes_out: u64,
}

impl TagsFilterOscStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "OSC filtered: create {}/{} kept, modify {}/{} kept, delete {}/{} preserved",
            self.creates_out,
            self.creates_in,
            self.modifies_out,
            self.modifies_in,
            self.deletes_out,
            self.deletes_in,
        )?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Filter an OSC file by tag expressions.
///
/// - `create` / `modify`: only matching elements are kept.
/// - `delete`: all deletes are forwarded unconditionally.
///
/// `parallelism` sizes the filtering workers, which run on
/// [`Host::executor`](crate::host::Host): `Fixed(n)` runs at most `n` of
/// them at once there, `Auto` leaves them unbounded. The
/// one reader thread that frames the document runs beside the workers at
/// any setting, `Fixed(1)` included. The output does not depend on it.
pub fn tags_filter_osc(
    input: &Path,
    output: impl crate::output::BindOutput,
    expression_strs: &[String],
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
) -> Result<TagsFilterOscStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = tags_filter_osc_to(
        input,
        &output,
        expression_strs,
        parallelism,
        stream::Batching::PRODUCTION,
        host,
    )?;
    output.publish()?;
    Ok(result)
}

/// [`tags_filter_osc`] writing to a borrowed output it does not publish.
///
/// The input is read on merge-changes' lexical readers
/// ([`stream::collect`]): one reader frames and checks the document and cuts
/// it into batches of whole elements, and pool workers check each batch's
/// elements with the shared parsers and render the kept ones (every delete,
/// and each create or modify with a matching tag) into nine byte groups,
/// one per action and kind. The document is then written once: each
/// non-empty action's section holds its node, way and relation groups, each
/// concatenated in input order. That is the order and the bytes the owned
/// writers produced (a differential test holds it to that), with no owned
/// element built for any change.
///
/// Memory is the rendered XML of the kept changes plus the readers' bounded
/// in-flight batches; a match-all filter retains the whole input's rendered
/// XML. Spooling the groups to scratch would bound that at the cost of a full
/// write and read of the kept output and a scratch owner this command does
/// not have.
#[hotpath::measure]
pub(crate) fn tags_filter_osc_to(
    input: &Path,
    output: &crate::output::BoundOutput,
    expression_strs: &[String],
    parallelism: crate::host::Parallelism,
    batching: stream::Batching,
    host: &crate::host::Entered<'_>,
) -> Result<TagsFilterOscStats> {
    let is_gz = crate::osc::output_is_gzip(output.destination())?;
    let expressions = parse_expressions(expression_strs)?;
    host.invocation.marker("TAGSFILTEROSC_START");
    // The workers run on the host's executor; a fixed parallelism caps how
    // many of them run at once there (`stream::Encoder`), not a pool of the
    // call's own. Under `Auto` they are unbounded.
    if let Some(n) = parallelism.fixed() {
        host.invocation.counter("tagsfilter_worker_count", n);
    }
    let inputs = [input];
    let plan = stream::Plan {
        batching,
        inputs: &inputs,
        is_gz: false,
        readers: 1,
        lanes: parallelism.fixed().unwrap_or(usize::MAX),
        executor: host.executor.borrowed(),
        cancel: &host.invocation,
    };
    let expressions = expressions.as_slice();
    let batches = stream::collect(&plan, &|batch: &LexBatch, path: &Path| {
        filter_batch(batch, path, expressions)
    })?;
    let batches: Vec<FilteredBatch> = batches.into_iter().flatten().collect();
    host.invocation.check()?;
    let mut stats = TagsFilterOscStats::default();
    for b in &batches {
        stats.creates_in += b.stats.creates_in;
        stats.creates_out += b.stats.creates_out;
        stats.modifies_in += b.stats.modifies_in;
        stats.modifies_out += b.stats.modifies_out;
        stats.deletes_in += b.stats.deletes_in;
        stats.deletes_out += b.stats.deletes_out;
    }
    write_groups(output, is_gz, &batches)?;
    host.invocation.marker("TAGSFILTEROSC_END");
    Ok(stats)
}

/// One batch's kept elements, rendered: groups in (action, kind) order,
/// create node first, delete relation last.
struct FilteredBatch {
    groups: [Vec<u8>; 9],
    stats: TagsFilterOscStats,
}

fn group_index(action: Section, kind: ElementKind) -> usize {
    let a = match action {
        Section::Create => 0,
        Section::Modify => 1,
        Section::Delete => 2,
    };
    let k = match kind {
        ElementKind::Node => 0,
        ElementKind::Way => 1,
        ElementKind::Relation => 2,
    };
    a * 3 + k
}

/// A writer whose indent state is the document's inside an action section
/// (depth 2, line break pending), with nothing written: an element rendered
/// into it has the bytes it has at that place in the document, whatever
/// precedes it there.
fn group_writer() -> Result<Writer<Vec<u8>>> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    writer.write_event(Event::Start(BytesStart::new("osmChange")))?;
    writer.write_event(Event::Start(BytesStart::new("create")))?;
    writer.get_mut().clear();
    Ok(writer)
}

fn filter_batch(
    batch: &LexBatch,
    path: &Path,
    expressions: &[Expression],
) -> Result<FilteredBatch> {
    let mut sink = FilterBytes {
        element: ElementBuf::new("tags-filter OSC"),
        groups: Vec::with_capacity(9),
        stats: TagsFilterOscStats::default(),
        expressions,
    };
    for _ in 0..9 {
        sink.groups.push(group_writer()?);
    }
    batch.replay(&mut sink, path)?;
    let mut groups: [Vec<u8>; 9] = Default::default();
    for (slot, writer) in groups.iter_mut().zip(sink.groups) {
        *slot = writer.into_inner();
    }
    Ok(FilteredBatch {
        groups,
        stats: sink.stats,
    })
}

/// The worker sink: every element checked as the owned sink checks it;
/// deletes kept unconditionally, creates and modifies when a tag matches.
struct FilterBytes<'a> {
    element: ElementBuf,
    groups: Vec<Writer<Vec<u8>>>,
    stats: TagsFilterOscStats,
    expressions: &'a [Expression],
}

impl OscSink for FilterBytes<'_> {
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;
    const END_ELEMENT_EMITS: bool = true;

    fn begin_element(
        &mut self,
        action: Section,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> Result<()> {
        self.element.begin(action, kind, e)
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> Result<()> {
        self.element.tag(e)
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> Result<()> {
        self.element.nd(e)
    }

    fn member(&mut self, e: &BytesStart<'_>) -> Result<()> {
        self.element.member(e)
    }

    fn end_element(&mut self) -> Result<()> {
        let Some((action, kind)) = self.element.finish() else {
            return Ok(());
        };
        let keep = match action {
            Section::Delete => {
                self.stats.deletes_in += 1;
                self.stats.deletes_out += 1;
                true
            }
            Section::Create => {
                self.stats.creates_in += 1;
                let keep = self.matches(kind);
                self.stats.creates_out += u64::from(keep);
                keep
            }
            Section::Modify => {
                self.stats.modifies_in += 1;
                let keep = self.matches(kind);
                self.stats.modifies_out += u64::from(keep);
                keep
            }
        };
        if keep {
            let writer = self
                .groups
                .get_mut(group_index(action, kind))
                .ok_or_else(|| crate::Error::internal("tags-filter OSC: no group writer"))?;
            self.element.write(writer, kind)?;
        }
        Ok(())
    }
}

impl FilterBytes<'_> {
    /// [`matches_any`] over the element's tags as read.
    fn matches(&self, kind: ElementKind) -> bool {
        self.element.tag_pairs().any(|(key, value)| {
            self.expressions
                .iter()
                .any(|expr| expr.type_filter.admits(kind) && tag_matches(&expr.matcher, key, value))
        })
    }
}

fn write_groups(
    output: &crate::output::BoundOutput,
    is_gz: bool,
    batches: &[FilteredBatch],
) -> Result<()> {
    let out = output.writer()?;
    let sink = if is_gz {
        OscByteSink::Gz(Box::new(std::io::BufWriter::with_capacity(
            crate::tunables::OSC_GZIP_WRITE_BUFFER,
            GzEncoder::new(out, flate2::Compression::fast()),
        )))
    } else {
        OscByteSink::Plain(out)
    };
    let mut writer = Writer::new_with_indent(sink, b' ', 2);
    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;
    for (a, action) in [Section::Create, Section::Modify, Section::Delete]
        .into_iter()
        .enumerate()
    {
        let range = a * 3..a * 3 + 3;
        let present = batches
            .iter()
            .any(|b| b.groups[range.clone()].iter().any(|g| !g.is_empty()));
        if !present {
            continue;
        }
        writer.write_event(Event::Start(BytesStart::new(action.tag())))?;
        // Raw bytes: a rendered element leaves the indent state as it found
        // it, so the closing tag indents as after event-by-event writing.
        for g in range {
            for b in batches {
                std::io::Write::write_all(writer.get_mut(), &b.groups[g])?;
            }
        }
        writer.write_event(Event::End(BytesEnd::new(action.tag())))?;
    }
    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    match writer.into_inner() {
        OscByteSink::Gz(buffered) => buffered
            .into_inner()
            .map_err(|e| crate::Error::from(e.into_error()))?
            .finish()?
            .finish(),
        OscByteSink::Plain(out) => out.finish(),
    }
}

#[cfg(test)]
fn parse_and_filter_osc(
    input: &Path,
    expressions: &[Expression],
    cancel: &crate::host::Recorder,
) -> Result<(FilteredOsc, TagsFilterOscStats)> {
    let mut sink = FilterSink {
        filtered: FilteredOsc::new(),
        stats: TagsFilterOscStats::default(),
        expressions,
        section: Section::Create,
        current: None,
    };
    // Deliberately emit merge_changes_decompress_ns for this file, just as the
    // other sinks do. Renaming or accumulating it breaks brokkr comparisons.
    parse_osc_path(input, &mut sink, cancel)?;
    Ok((sink.filtered, sink.stats))
}

/// Sink that keeps the create/modify elements matching `expressions`
/// and forwards every delete unconditionally.
#[cfg(test)]
struct FilterSink<'a> {
    filtered: FilteredOsc,
    stats: TagsFilterOscStats,
    expressions: &'a [Expression],
    section: Section,
    current: Option<CurrentElem>,
}

#[cfg(test)]
impl FilterSink<'_> {
    /// The element a child belongs to; every object, deleted ones included.
    fn current(&mut self) -> Result<&mut CurrentElem> {
        self.current.as_mut().ok_or_else(|| {
            crate::Error::internal("tags-filter OSC: an object child with no current object")
        })
    }
}

#[cfg(test)]
impl OscSink for FilterSink<'_> {
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;

    fn begin_element(
        &mut self,
        action: Section,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> Result<()> {
        let id = parse_i64_attr(e, "id")?;
        self.section = action;
        // Read on every action, delete included: a `<delete>` carries
        // the version of the state it supersedes, and is written back with
        // it. Dropping it here is how this filter used to turn a
        // metadata-bearing change stream into an anonymous one.
        let metadata = parse_owned_metadata(e)?;
        match action {
            Section::Create => self.stats.creates_in += 1,
            Section::Modify => self.stats.modifies_in += 1,
            Section::Delete => self.stats.deletes_in += 1,
        }

        let mut cur = CurrentElem::new(kind, id, metadata);
        if kind == ElementKind::Node {
            cur.location = if action == Section::Delete {
                delete_coords(e, id)?
            } else {
                Some((
                    coord_to_decimicro(e, "lat", id)?,
                    coord_to_decimicro(e, "lon", id)?,
                ))
            };
        }
        self.current = Some(cur);
        Ok(())
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> Result<()> {
        let tag = parse_tag(e)?;
        self.current()?.tags.push(tag);
        Ok(())
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> Result<()> {
        let id = parse_nd(e)?;
        self.current()?.refs.push(id);
        Ok(())
    }

    fn member(&mut self, e: &BytesStart<'_>) -> Result<()> {
        // The shared parser admits only node/way/relation, so no
        // `MemberId::Unknown` is ever built here (`osc::write` refuses to
        // emit one).
        let member = parse_member(e)?;
        let id = member.member_id();
        self.current()?.members.push(OwnedMember {
            id,
            role: member.role,
        });
        Ok(())
    }

    fn end_element(&mut self) -> Result<()> {
        let section = self.section;
        let expressions = self.expressions;
        finalize_current(
            section,
            &mut self.current,
            &mut self.filtered,
            &mut self.stats,
            expressions,
        );
        Ok(())
    }
}

#[cfg(test)]
fn finalize_current(
    section: Section,
    current: &mut Option<CurrentElem>,
    filtered: &mut FilteredOsc,
    stats: &mut TagsFilterOscStats,
    expressions: &[Expression],
) {
    let Some(cur) = current.take() else {
        return;
    };
    // Every delete is kept, body and all; a create or modify only when a
    // tag matches.
    if section == Section::Delete {
        stats.deletes_out += 1;
        match cur.kind {
            ElementKind::Node => filtered.delete_nodes.push(OwnedNode {
                id: cur.id,
                location: cur.location,
                tags: cur.tags,
                metadata: cur.metadata,
            }),
            ElementKind::Way => filtered.delete_ways.push(OwnedWay {
                id: cur.id,
                tags: cur.tags,
                refs: cur.refs,
                metadata: cur.metadata,
            }),
            ElementKind::Relation => filtered.delete_relations.push(OwnedRelation {
                id: cur.id,
                tags: cur.tags,
                members: cur.members,
                metadata: cur.metadata,
            }),
        }
        return;
    }
    if !matches_any(expressions, &cur.tags, cur.kind) {
        return;
    }

    match (section, cur.kind) {
        (Section::Create, ElementKind::Node) => {
            stats.creates_out += 1;
            filtered.create_nodes.push(OwnedNode {
                id: cur.id,
                location: cur.location,
                tags: cur.tags,
                metadata: cur.metadata,
            });
        }
        (Section::Create, ElementKind::Way) => {
            stats.creates_out += 1;
            filtered.create_ways.push(OwnedWay {
                id: cur.id,
                tags: cur.tags,
                refs: cur.refs,
                metadata: cur.metadata,
            });
        }
        (Section::Create, ElementKind::Relation) => {
            stats.creates_out += 1;
            filtered.create_relations.push(OwnedRelation {
                id: cur.id,
                tags: cur.tags,
                members: cur.members,
                metadata: cur.metadata,
            });
        }
        (Section::Modify, ElementKind::Node) => {
            stats.modifies_out += 1;
            filtered.modify_nodes.push(OwnedNode {
                id: cur.id,
                location: cur.location,
                tags: cur.tags,
                metadata: cur.metadata,
            });
        }
        (Section::Modify, ElementKind::Way) => {
            stats.modifies_out += 1;
            filtered.modify_ways.push(OwnedWay {
                id: cur.id,
                tags: cur.tags,
                refs: cur.refs,
                metadata: cur.metadata,
            });
        }
        (Section::Modify, ElementKind::Relation) => {
            stats.modifies_out += 1;
            filtered.modify_relations.push(OwnedRelation {
                id: cur.id,
                tags: cur.tags,
                members: cur.members,
                metadata: cur.metadata,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
fn write_filtered_osc(output: &crate::output::BoundOutput, filtered: &FilteredOsc) -> Result<()> {
    // Framing follows the output name, the rule every OSC producer shares
    // (`crate::osc::output_is_gzip`); the name was vetted before any work.
    let out = output.writer()?;
    let sink = if crate::osc::output_is_gzip(output.destination())? {
        OscByteSink::Gz(Box::new(std::io::BufWriter::with_capacity(
            crate::tunables::OSC_GZIP_WRITE_BUFFER,
            GzEncoder::new(out, flate2::Compression::fast()),
        )))
    } else {
        OscByteSink::Plain(out)
    };
    let mut writer = Writer::new_with_indent(sink, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    if !filtered.create_nodes.is_empty()
        || !filtered.create_ways.is_empty()
        || !filtered.create_relations.is_empty()
    {
        writer.write_event(Event::Start(BytesStart::new("create")))?;
        for node in &filtered.create_nodes {
            write_node_xml(&mut writer, node)?;
        }
        for way in &filtered.create_ways {
            write_way_xml(&mut writer, way)?;
        }
        for rel in &filtered.create_relations {
            write_relation_xml(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("create")))?;
    }

    if !filtered.modify_nodes.is_empty()
        || !filtered.modify_ways.is_empty()
        || !filtered.modify_relations.is_empty()
    {
        writer.write_event(Event::Start(BytesStart::new("modify")))?;
        for node in &filtered.modify_nodes {
            write_node_xml(&mut writer, node)?;
        }
        for way in &filtered.modify_ways {
            write_way_xml(&mut writer, way)?;
        }
        for rel in &filtered.modify_relations {
            write_relation_xml(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("modify")))?;
    }

    if !filtered.delete_nodes.is_empty()
        || !filtered.delete_ways.is_empty()
        || !filtered.delete_relations.is_empty()
    {
        writer.write_event(Event::Start(BytesStart::new("delete")))?;
        for node in &filtered.delete_nodes {
            write_node_xml(&mut writer, node)?;
        }
        for way in &filtered.delete_ways {
            write_way_xml(&mut writer, way)?;
        }
        for rel in &filtered.delete_relations {
            write_relation_xml(&mut writer, rel)?;
        }
        writer.write_event(Event::End(BytesEnd::new("delete")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    // Finish the gzip stream, then the output's writer, whose checked flush
    // is what lets the output be published.
    match writer.into_inner() {
        OscByteSink::Gz(buffered) => buffered
            .into_inner()
            .map_err(|e| crate::Error::from(e.into_error()))?
            .finish()?
            .finish(),
        OscByteSink::Plain(out) => out.finish(),
    }
}

/// The OSC document's byte sink: gzip-framed or plain, per the output name.
/// The gzip side is buffered in front of the encoder: the XML writer emits
/// many tiny writes per element, and deflate run once per write was most of
/// such a writer's time.
enum OscByteSink {
    Gz(Box<std::io::BufWriter<GzEncoder<crate::output::OutputWriter>>>),
    Plain(crate::output::OutputWriter),
}

impl std::io::Write for OscByteSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Gz(w) => std::io::Write::write(w, buf),
            Self::Plain(w) => std::io::Write::write(w, buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Gz(w) => std::io::Write::flush(w),
            Self::Plain(w) => std::io::Write::flush(w),
        }
    }
}

// The local `write_node` / `write_way` / `write_relation` /
// `write_delete_id` that used to sit here are gone. They were
// near-copies of `osc::write`'s writers that had drifted in two ways
// nobody intended: they dropped element metadata entirely (a filter that
// erases version, timestamp, changeset, uid and user from every change
// it keeps), and they pushed tag keys, tag values and member roles as
// raw attributes rather than through `push_attribute_escaped`, so a tag
// value containing a quote or an ampersand produced malformed XML here
// and well-formed XML from `merge-changes`. Same input, two answers -
// the exact defect this hunt was about, one layer above the parser.

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn bound(path: &Path) -> crate::output::BoundOutput {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        crate::output::bind_unissued(
            crate::output::Output::new(
                path,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )
            .unwrap(),
            &host.invocation,
        )
        .unwrap()
    }

    fn owned(input: &Path, output: &Path, exprs: &[String]) -> (Vec<u8>, String) {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let out = bound(output);
        let expressions = parse_expressions(exprs).unwrap();
        let (filtered, stats) =
            parse_and_filter_osc(input, &expressions, &host.invocation).unwrap();
        write_filtered_osc(&out, &filtered).unwrap();
        out.publish().unwrap();
        let mut summary = Vec::new();
        stats.write_summary(&mut summary).unwrap();
        (
            std::fs::read(output).unwrap(),
            String::from_utf8(summary).unwrap(),
        )
    }

    /// How a differential run is scheduled: a fixed pool of the call's own,
    /// or `Auto` on the ambient executor or on a caller's one-thread pool.
    #[derive(Clone, Copy, Debug)]
    enum Schedule {
        Fixed(usize),
        AutoAmbient,
        AutoCallerPool,
    }

    const SCHEDULES: [Schedule; 5] = [
        Schedule::Fixed(1),
        Schedule::Fixed(2),
        Schedule::Fixed(8),
        Schedule::AutoAmbient,
        Schedule::AutoCallerPool,
    ];

    fn filter_with(
        input: &Path,
        output: &Path,
        exprs: &[String],
        schedule: Schedule,
    ) -> Result<TagsFilterOscStats> {
        let mut host = crate::host::Host::for_tests();
        let parallelism = match schedule {
            Schedule::Fixed(n) => crate::host::Parallelism::from_jobs(n),
            Schedule::AutoAmbient => {
                // Auto on the CLI's executor, rayon's ambient pool.
                host.executor = crate::host::Executor::Ambient; // ambient-deliberate: the CLI's executor
                crate::host::Parallelism::Auto
            }
            Schedule::AutoCallerPool => {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .map_err(|e| crate::Error::internal(e.to_string()))?;
                host.executor = crate::host::Executor::Pool(std::sync::Arc::new(pool));
                crate::host::Parallelism::Auto
            }
        };
        // `tags_filter_osc` at small batches, so a few hundred elements
        // make dozens of batches with several in flight.
        let host = host.enter()?;
        let output = crate::output::bind_unissued(
            crate::output::Output::new(
                output,
                crate::output::Overwrite::Replace,
                &crate::host::Diagnostics::discard(),
            )?,
            &host.invocation,
        )?;
        let batching = stream::Batching {
            batch_bytes: 1 << 10,
            inflight_bytes: 8 << 10,
        };
        let stats = tags_filter_osc_to(input, &output, exprs, parallelism, batching, &host)?;
        output.publish()?;
        Ok(stats)
    }

    fn rendered(
        input: &Path,
        output: &Path,
        exprs: &[String],
        schedule: Schedule,
    ) -> (Vec<u8>, String) {
        let stats = filter_with(input, output, exprs, schedule).unwrap();
        let mut summary = Vec::new();
        stats.write_summary(&mut summary).unwrap();
        (
            std::fs::read(output).unwrap(),
            String::from_utf8(summary).unwrap(),
        )
    }

    fn decompress(bytes: &[u8]) -> String {
        use std::io::Read;
        let mut xml = String::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .read_to_string(&mut xml)
            .unwrap();
        xml
    }

    /// Kinds and actions interleaved inside sections, over many batches,
    /// with metadata, escapes and children before and after the matching
    /// tag: the byte path writes the owned path's document.
    #[test]
    fn byte_path_writes_the_owned_document() {
        let dir = crate::path_guard::tests::scratch();
        let count = 400_i64;
        let mut body = String::new();
        for k in 0..count {
            let action = match k % 4 {
                0 | 3 => "create",
                1 => "modify",
                _ => "delete",
            };
            let meta = if k % 5 == 0 {
                String::new()
            } else {
                format!(" version=\"{}\" user=\"a&amp;{k}\"", k % 7)
            };
            let tag = if k % 3 == 0 { "primary" } else { "residential" };
            let element = match k % 3 {
                0 => format!(
                    "<node id=\"{k}\"{meta} lat=\"1.5\" lon=\"2\"><tag k=\"highway\" v=\"{tag}\"/></node>"
                ),
                1 => format!(
                    "<way id=\"{k}\"{meta}><nd ref=\"1\"/><tag k=\"highway\" v=\"{tag}\"/><nd ref=\"{k}\"/></way>"
                ),
                _ => format!(
                    "<relation id=\"{k}\"{meta}><tag k=\"type\" v=\"&lt;r\"/><member type=\"node\" ref=\"{k}\" role=\"x\"/></relation>"
                ),
            };
            body.push_str(&format!("<{action}>{element}</{action}>"));
        }
        let input = dir.path().join("in.osc");
        std::fs::write(
            &input,
            format!("<osmChange version=\"0.6\">{body}</osmChange>"),
        )
        .unwrap();
        for exprs in [
            vec!["highway=primary".to_string()],
            vec!["w/highway".to_string(), "r/type".to_string()],
            vec!["highway!=primary".to_string()],
            vec!["nothing=here".to_string()],
        ] {
            let plain = dir.path().join("out.osc");
            let a = owned(&input, &plain, &exprs);
            let gz = dir.path().join("out.osc.gz");
            let (a_bytes, a_stats) = owned(&input, &gz, &exprs);
            for schedule in SCHEDULES {
                let b = rendered(&input, &plain, &exprs, schedule);
                assert!(a == b, "{exprs:?} {schedule:?}");
                let (b_bytes, b_stats) = rendered(&input, &gz, &exprs, schedule);
                assert_eq!(a_stats, b_stats, "{exprs:?} {schedule:?}");
                assert!(
                    decompress(&a_bytes) == decompress(&b_bytes),
                    "{exprs:?} {schedule:?}"
                );
            }
        }
    }

    /// Many batches of well-formed elements, then `tail`.
    fn many_batches_then(dir: &Path, name: &str, tail: &str, close: bool) -> std::path::PathBuf {
        let count = 400;
        let mut body = String::from("<osmChange version=\"0.6\"><create>");
        for k in 0..count {
            body.push_str(&format!(
                "<node id=\"{k}\" lat=\"1\" lon=\"2\"><tag k=\"highway\" v=\"primary\"/></node>"
            ));
        }
        body.push_str(tail);
        if close {
            body.push_str("</create></osmChange>");
        }
        let input = dir.join(name);
        std::fs::write(&input, body).unwrap();
        input
    }

    /// A defect after many good batches fails the run at every schedule,
    /// one worker included: one the workers' replay refuses (a coordinate
    /// the lexer frames but does not read) and one the reader refuses (a
    /// truncated document).
    #[test]
    fn a_late_defect_fails_the_run_at_every_schedule() {
        let dir = crate::path_guard::tests::scratch();
        let exprs = ["highway".to_string()];
        let replay = many_batches_then(
            dir.path(),
            "replay.osc",
            "<node id=\"-1\" lat=\"north\" lon=\"2\"/>",
            true,
        );
        let truncated = many_batches_then(
            dir.path(),
            "truncated.osc",
            "<node id=\"-2\" lat=\"1\"",
            false,
        );
        let out = dir.path().join("out.osc");
        for schedule in SCHEDULES {
            let err = filter_with(&replay, &out, &exprs, schedule).unwrap_err();
            assert!(err.to_string().contains("node -1"), "{schedule:?}: {err}");
            assert!(
                filter_with(&truncated, &out, &exprs, schedule).is_err(),
                "{schedule:?}"
            );
        }
    }
}
