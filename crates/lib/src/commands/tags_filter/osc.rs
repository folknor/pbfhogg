//! Filter OSC files by tag expressions while preserving all delete actions.

use std::io::Write as _;
use std::path::Path;

#[cfg(test)]
use flate2::write::GzEncoder;
#[cfg(test)]
use quick_xml::Writer;
#[cfg(test)]
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use crate::ElementKind;
use crate::commands::merge_changes;
use crate::commands::merge_changes::emit::ElementBuf;
use crate::commands::merge_changes::stream;
use crate::error::Result;
#[cfg(test)]
use crate::osc::write::{
    OwnedMember, OwnedNode, OwnedRelation, OwnedWay, write_node_xml, write_relation_xml,
    write_way_xml,
};
#[cfg(test)]
use crate::osc::xml_parse::{DeleteChildren, OscSink};
use crate::osc::xml_parse::{LexBatch, OscAction};
#[cfg(test)]
use crate::osc::xml_parse::{
    node_coords, parse_i64_attr, parse_member, parse_nd, parse_osc_path, parse_owned_metadata,
    parse_tag,
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

/// One kept change of the owned oracle.
#[cfg(test)]
enum KeptElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

/// The kept changes in input order, each with its action.
#[cfg(test)]
type FilteredOsc = Vec<(Section, KeptElement)>;

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TagsFilterOscStats {
    pub creates_in: u64,
    pub creates_out: u64,
    pub modifies_in: u64,
    pub modifies_out: u64,
    pub deletes_in: u64,
    pub deletes_out: u64,
}

impl std::ops::AddAssign for TagsFilterOscStats {
    fn add_assign(&mut self, other: Self) {
        self.creates_in += other.creates_in;
        self.creates_out += other.creates_out;
        self.modifies_in += other.modifies_in;
        self.modifies_out += other.modifies_out;
        self.deletes_in += other.deletes_in;
        self.deletes_out += other.deletes_out;
    }
}

impl stream::Tally for TagsFilterOscStats {
    /// The per-input change count is merge's counter, not the filter's.
    fn changes_per_osc(&self) -> Option<u64> {
        None
    }
}

impl TagsFilterOscStats {
    /// Count one checked change of `action`, kept or not.
    fn record(&mut self, action: Section, kept: bool) {
        let (seen, out) = match action {
            Section::Create => (&mut self.creates_in, &mut self.creates_out),
            Section::Modify => (&mut self.modifies_in, &mut self.modifies_out),
            Section::Delete => (&mut self.deletes_in, &mut self.deletes_out),
        };
        *seen += 1;
        *out += u64::from(kept);
    }

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
/// The input runs through merge-changes' streaming path ([`stream::run`]):
/// one reader frames and checks the document and cuts it into batches of
/// whole elements, and workers check every element of a batch with the
/// shared parsers and render the kept ones (every delete, and each create or
/// modify with a matching tag) in input order, each batch opening and
/// closing its own action sections and, for `.osc.gz`, deflated as its own
/// gzip member. The batches are written in order as they finish.
///
/// Input order is the contract, not a layout choice: an apply admits an
/// equal version and lets the later change win, so a filter that grouped
/// the kept changes by action would turn a delete followed by a same-version
/// re-create into a re-create followed by a delete, and a delete followed by
/// a higher-version create into a version regression. Kept changes keep
/// their relative order, so every surviving claim reaches an apply as the
/// unfiltered stream would have delivered it.
///
/// Memory is the readers' bounded in-flight batches; nothing of the output
/// is retained.
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
        is_gz,
        readers: 1,
        lanes: parallelism.fixed().unwrap_or(usize::MAX),
        executor: host.executor.borrowed(),
        cancel: &host.invocation,
    };
    let expressions = expressions.as_slice();
    let encode = |batch: &LexBatch, path: &Path| filter_batch(batch, path, is_gz, expressions);
    let mut out = output.writer()?;
    out.write_all(&merge_changes::build_prelude_bytes(is_gz)?)?;
    // One input never spills, so the run needs no scratch.
    let stats = stream::run(&plan, None, &mut out, &encode)?;
    host.invocation.check()?;
    out.write_all(&merge_changes::build_postlude_bytes(is_gz)?)?;
    out.finish()?;
    host.invocation.marker("TAGSFILTEROSC_END");
    Ok(stats)
}

/// One batch, filtered and rendered in input order: every delete, and each
/// create or modify with a tag an expression admits for its kind.
fn filter_batch(
    batch: &LexBatch,
    path: &Path,
    is_gz: bool,
    expressions: &[Expression],
) -> Result<(Vec<u8>, TagsFilterOscStats)> {
    let mut stats = TagsFilterOscStats::default();
    let mut keep = |action: Section, kind: ElementKind, element: &ElementBuf| {
        let kept = action == Section::Delete || matches(expressions, kind, element);
        stats.record(action, kept);
        kept
    };
    let bytes =
        merge_changes::encode_batch_filtered(batch, path, is_gz, "tags-filter OSC", &mut keep)?;
    Ok((bytes, stats))
}

/// Whether any of the element's tags, as read, matches an expression that
/// admits its kind.
fn matches(expressions: &[Expression], kind: ElementKind, element: &ElementBuf) -> bool {
    element.tag_pairs().any(|(key, value)| {
        expressions
            .iter()
            .any(|expr| expr.type_filter.admits(kind) && tag_matches(&expr.matcher, key, value))
    })
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
        let hidden = metadata.as_ref().is_some_and(crate::osc::write::OwnedMetadata::hidden);

        let mut cur = CurrentElem::new(kind, id, metadata);
        if kind == ElementKind::Node {
            // A hidden create or modify is the object's deleted state: its
            // location is optional, as a delete's is.
            cur.location = node_coords(e, id, action == Section::Delete || hidden)?;
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
    let kept = section == Section::Delete || matches_any(expressions, &cur.tags, cur.kind);
    stats.record(section, kept);
    if !kept {
        return;
    }
    let element = match cur.kind {
        ElementKind::Node => KeptElement::Node(OwnedNode {
            id: cur.id,
            location: cur.location,
            tags: cur.tags,
            metadata: cur.metadata,
        }),
        ElementKind::Way => KeptElement::Way(OwnedWay {
            id: cur.id,
            tags: cur.tags,
            refs: cur.refs,
            metadata: cur.metadata,
        }),
        ElementKind::Relation => KeptElement::Relation(OwnedRelation {
            id: cur.id,
            tags: cur.tags,
            members: cur.members,
            metadata: cur.metadata,
        }),
    };
    filtered.push((section, element));
}

/// The owned writer: one document, the kept changes in input order, an
/// action section opened at each change of action.
#[cfg(test)]
fn write_filtered_osc(output: &crate::output::BoundOutput, filtered: &FilteredOsc) -> Result<()> {
    // Framing follows the output name, the rule every OSC producer shares
    // (`crate::osc::output_is_gzip`); the name was vetted before any work.
    let out = output.writer()?;
    let sink = if crate::osc::output_is_gzip(output.destination())? {
        OscByteSink::Gz(Box::new(std::io::BufWriter::new(GzEncoder::new(
            out,
            flate2::Compression::fast(),
        ))))
    } else {
        OscByteSink::Plain(out)
    };
    let mut writer = Writer::new_with_indent(sink, b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;
    let mut root = BytesStart::new("osmChange");
    root.push_attribute(("version", "0.6"));
    writer.write_event(Event::Start(root))?;

    let mut open: Option<Section> = None;
    for (section, element) in filtered {
        if open != Some(*section) {
            if let Some(prev) = open {
                writer.write_event(Event::End(BytesEnd::new(prev.tag())))?;
            }
            writer.write_event(Event::Start(BytesStart::new(section.tag())))?;
            open = Some(*section);
        }
        match element {
            KeptElement::Node(node) => write_node_xml(&mut writer, node)?,
            KeptElement::Way(way) => write_way_xml(&mut writer, way)?,
            KeptElement::Relation(rel) => write_relation_xml(&mut writer, rel)?,
        }
    }
    if let Some(prev) = open {
        writer.write_event(Event::End(BytesEnd::new(prev.tag())))?;
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

/// The owned writer's byte sink: gzip-framed or plain, per the output name.
/// The gzip side is buffered in front of the encoder: the XML writer emits
/// many tiny writes per element, and deflate run once per write was most of
/// such a writer's time.
#[cfg(test)]
enum OscByteSink {
    Gz(Box<std::io::BufWriter<GzEncoder<crate::output::OutputWriter>>>),
    Plain(crate::output::OutputWriter),
}

#[cfg(test)]
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
            let (a_plain, a_stats) = owned(&input, &plain, &exprs);
            let a_xml = normal_form(&String::from_utf8(a_plain).unwrap());
            let gz = dir.path().join("out.osc.gz");
            let (a_gz, _) = owned(&input, &gz, &exprs);
            assert_eq!(a_xml, normal_form(&decompress(&a_gz)), "{exprs:?}");
            let mut first: Option<(Vec<u8>, Vec<u8>)> = None;
            for schedule in SCHEDULES {
                let (b_plain, b_stats) = rendered(&input, &plain, &exprs, schedule);
                assert_eq!(a_stats, b_stats, "{exprs:?} {schedule:?}");
                let b_xml = normal_form(&String::from_utf8(b_plain.clone()).unwrap());
                assert!(a_xml == b_xml, "{exprs:?} {schedule:?}");
                let (b_gz, _) = rendered(&input, &gz, &exprs, schedule);
                assert!(
                    a_xml == normal_form(&decompress(&b_gz)),
                    "{exprs:?} {schedule:?}"
                );
                // The bytes, gzip members included, do not depend on the
                // schedule.
                match &first {
                    None => first = Some((b_plain, b_gz)),
                    Some((p, g)) => {
                        assert!(*p == b_plain && *g == b_gz, "{exprs:?} {schedule:?}");
                    }
                }
            }
        }
    }

    /// The document with inter-element whitespace removed and each run of
    /// same-action sections joined into one: what is left is the changes,
    /// their actions and their order.
    fn normal_form(xml: &str) -> String {
        let mut out = String::new();
        let mut gap = String::new();
        let mut in_tag = false;
        for c in xml.chars() {
            if in_tag {
                out.push(c);
                in_tag = c != '>';
            } else if c == '<' {
                gap.clear();
                out.push(c);
                in_tag = true;
            } else {
                gap.push(c);
                assert!(c.is_whitespace(), "text outside a tag: {gap:?}");
            }
        }
        for action in ["create", "modify", "delete"] {
            out = out.replace(&format!("</{action}><{action}>"), "");
        }
        out
    }

    /// The ids and actions of a document's changes, in order.
    fn changes(xml: &str) -> Vec<String> {
        let xml = normal_form(xml);
        let mut action = "";
        let mut out = Vec::new();
        for piece in xml.split('<').skip(1) {
            let name = piece.split([' ', '>', '/']).next().unwrap_or("");
            match name {
                "create" | "modify" | "delete" => action = name,
                "node" | "way" | "relation" => {
                    let id = piece
                        .split("id=\"")
                        .nth(1)
                        .unwrap()
                        .split('"')
                        .next()
                        .unwrap();
                    let version = piece
                        .split("version=\"")
                        .nth(1)
                        .map_or("-", |v| v.split('"').next().unwrap());
                    out.push(format!("{action} {name} {id} v{version}"));
                }
                _ => {}
            }
        }
        out
    }

    /// A delete followed by a matching re-create of the same object keeps
    /// that order, at the same version and at a higher one, across batch
    /// boundaries with a batch that keeps nothing in between, and an
    /// interleaving of kinds and actions keeps its encounter order.
    #[test]
    fn kept_changes_keep_their_input_order() {
        let dir = crate::path_guard::tests::scratch();
        let mut body = String::from(
            "<osmChange version=\"0.6\">\
             <delete><node id=\"1\" version=\"3\" lat=\"1\" lon=\"2\"/>\
             <node id=\"2\" version=\"3\" lat=\"1\" lon=\"2\"/></delete>\
             <create><way id=\"9\" version=\"1\"><nd ref=\"1\"/><tag k=\"highway\" v=\"x\"/></way></create>",
        );
        // Enough dropped creates to fill whole batches at the test batching.
        body.push_str("<create>");
        for k in 100..200 {
            body.push_str(&format!(
                "<node id=\"{k}\" version=\"1\" lat=\"1\" lon=\"2\"><tag k=\"name\" v=\"n\"/></node>"
            ));
        }
        body.push_str("</create>");
        body.push_str(
            "<create><node id=\"1\" version=\"3\" lat=\"1\" lon=\"2\"><tag k=\"highway\" v=\"x\"/></node>\
             <node id=\"2\" version=\"4\" lat=\"1\" lon=\"2\"><tag k=\"highway\" v=\"x\"/></node></create>\
             <modify><way id=\"9\" version=\"2\"><nd ref=\"1\"/><tag k=\"highway\" v=\"y\"/></way></modify>\
             <delete><relation id=\"5\" version=\"2\"/></delete>\
             <modify><node id=\"1\" version=\"4\" lat=\"1\" lon=\"2\"><tag k=\"highway\" v=\"x\"/></node></modify>\
             </osmChange>",
        );
        let input = dir.path().join("in.osc");
        std::fs::write(&input, body).unwrap();
        let exprs = ["highway".to_string()];
        let expected = [
            "delete node 1 v3",
            "delete node 2 v3",
            "create way 9 v1",
            "create node 1 v3",
            "create node 2 v4",
            "modify way 9 v2",
            "delete relation 5 v2",
            "modify node 1 v4",
        ];
        let plain = dir.path().join("out.osc");
        let gz = dir.path().join("out.osc.gz");
        for schedule in SCHEDULES {
            let stats = filter_with(&input, &plain, &exprs, schedule).unwrap();
            let xml = String::from_utf8(std::fs::read(&plain).unwrap()).unwrap();
            assert_eq!(changes(&xml), expected, "{schedule:?}");
            assert_eq!(
                (stats.creates_in, stats.creates_out, stats.deletes_in),
                (103, 3, 3),
                "{schedule:?}"
            );
            filter_with(&input, &gz, &exprs, schedule).unwrap();
            let xml = decompress(&std::fs::read(&gz).unwrap());
            assert_eq!(changes(&xml), expected, "{schedule:?}");
        }
    }

    /// Applying the filtered stream leaves the re-created object alive, as
    /// applying the unfiltered one does: the overlay sees the delete first.
    #[test]
    fn a_same_version_recreate_survives_the_filter() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("in.osc");
        std::fs::write(
            &input,
            "<osmChange version=\"0.6\">\
             <delete><node id=\"1\" version=\"3\" lat=\"1\" lon=\"2\"/></delete>\
             <create><node id=\"1\" version=\"3\" lat=\"1\" lon=\"2\"><tag k=\"highway\" v=\"x\"/></node></create>\
             </osmChange>",
        )
        .unwrap();
        let out = dir.path().join("out.osc");
        filter_with(&input, &out, &["highway".to_string()], Schedule::Fixed(2)).unwrap();
        let diff = crate::osc::parse::parse_osc_file(&out).unwrap();
        assert!(diff.has_node(1), "the re-created node is live");
        assert!(!diff.is_deleted(ElementKind::Node, 1), "and not deleted");
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
