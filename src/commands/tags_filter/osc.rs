//! Filter OSC files by tag expressions while preserving all delete actions.

use std::path::Path;

use flate2::write::GzEncoder;
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};

use crate::error::Result;
use crate::osc::write::{
    OwnedMember, OwnedMetadata, OwnedNode, OwnedRelation, OwnedWay, write_delete_xml,
    write_node_xml, write_relation_xml, write_way_xml,
};
use crate::osc::xml_parse::{
    OscAction, OscSink, coord_to_decimicro, parse_i64_attr, parse_member_type, parse_osc_path,
    parse_owned_metadata, parse_str_attr, parse_str_attr_optional,
};
use crate::tag_expr::{Expression, parse_expressions, tag_matches};
use crate::{ElementKind, MemberId};

// This tag-first traversal borrows owned OSC tags without building a
// temporary borrowed-tag vector.
// It selects an entire create/modify element; deletes bypass this predicate.
// An empty expression list matches nothing, unlike export's optional filter.
// Single-tag wildcard and negation semantics remain owned by tag_matches.
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

struct CurrentElem {
    kind: ElementKind,
    id: i64,
    decimicro_lat: i32,
    decimicro_lon: i32,
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

impl CurrentElem {
    fn new(kind: ElementKind, id: i64, metadata: Option<crate::osc::write::OwnedMetadata>) -> Self {
        Self {
            kind,
            id,
            decimicro_lat: 0,
            decimicro_lon: 0,
            tags: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
            metadata,
        }
    }
}

// Nine action/kind vectors retain only matching creates/modifies, but every
// delete (with metadata). Memory is O(retained payload + largest input element),
// not O(compressed input bytes); a match-all filter can retain the whole OSC.
// This grouping preserves create/modify/delete then node/way/relation order,
// including encounter order within each group, through one gzip encoder.
// Three action spools alone cannot preserve kind order for interleaved input:
// a bounded replacement needs nine spools (or offsets plus a reorder pass).
// That adds a full retained-XML scratch write/read and needs scratch ownership,
// cleanup and disk-failure handling. Keep this measured in-memory path until
// match-all/catch-up RSS warrants that trade; simply emitting on encounter would
// change XML ordering, and concatenating gzip spools changes member bytes.
struct FilteredOsc {
    create_nodes: Vec<OwnedNode>,
    create_ways: Vec<OwnedWay>,
    create_relations: Vec<OwnedRelation>,
    modify_nodes: Vec<OwnedNode>,
    modify_ways: Vec<OwnedWay>,
    modify_relations: Vec<OwnedRelation>,
    delete_node_ids: Vec<(i64, Option<OwnedMetadata>)>,
    delete_way_ids: Vec<(i64, Option<OwnedMetadata>)>,
    delete_relation_ids: Vec<(i64, Option<OwnedMetadata>)>,
}

impl FilteredOsc {
    fn new() -> Self {
        Self {
            create_nodes: Vec::new(),
            create_ways: Vec::new(),
            create_relations: Vec::new(),
            modify_nodes: Vec::new(),
            modify_ways: Vec::new(),
            modify_relations: Vec::new(),
            delete_node_ids: Vec::new(),
            delete_way_ids: Vec::new(),
            delete_relation_ids: Vec::new(),
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
pub fn tags_filter_osc(
    input: &Path,
    output: crate::output::Output,
    expression_strs: &[String],
    host: &crate::host::Host,
) -> Result<TagsFilterOscStats> {
    let result = tags_filter_osc_to(input, &output, expression_strs, host)?;
    output.publish()?;
    Ok(result)
}

/// [`tags_filter_osc`] writing to a borrowed output it does not publish.
#[hotpath::measure]
pub(crate) fn tags_filter_osc_to(
    input: &Path,
    output: &crate::output::Output,
    expression_strs: &[String],
    host: &crate::host::Host,
) -> Result<TagsFilterOscStats> {
    let _ = host;
    let expressions = parse_expressions(expression_strs)?;
    crate::debug::emit_marker("TAGSFILTEROSC_START");
    let (filtered, stats) = parse_and_filter_osc(input, &expressions)?;
    write_filtered_osc(output, &filtered)?;
    crate::debug::emit_marker("TAGSFILTEROSC_END");
    Ok(stats)
}

fn parse_and_filter_osc(
    input: &Path,
    expressions: &[Expression],
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
    parse_osc_path(input, &mut sink)?;
    Ok((sink.filtered, sink.stats))
}

/// Sink that keeps the create/modify elements matching `expressions`
/// and forwards every delete unconditionally.
struct FilterSink<'a> {
    filtered: FilteredOsc,
    stats: TagsFilterOscStats,
    expressions: &'a [Expression],
    section: Section,
    current: Option<CurrentElem>,
}

impl OscSink for FilterSink<'_> {
    fn begin_element(
        &mut self,
        action: Section,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> Result<()> {
        let id = parse_i64_attr(e, "id")?;
        self.section = action;
        // Read on every action, delete included: a `<delete>` carries
        // the version of the state it supersedes, and the shared
        // `write_delete_xml` emits it. Dropping it here is how this
        // filter used to turn a metadata-bearing change stream into an
        // anonymous one.
        let metadata = parse_owned_metadata(e)?;

        if action == Section::Delete {
            self.stats.deletes_in += 1;
            self.stats.deletes_out += 1;
            match kind {
                ElementKind::Node => self.filtered.delete_node_ids.push((id, metadata)),
                ElementKind::Way => self.filtered.delete_way_ids.push((id, metadata)),
                ElementKind::Relation => self.filtered.delete_relation_ids.push((id, metadata)),
            }
            self.current = None;
            return Ok(());
        }
        if action == Section::Create {
            self.stats.creates_in += 1;
        } else {
            self.stats.modifies_in += 1;
        }

        let mut cur = CurrentElem::new(kind, id, metadata);
        if kind == ElementKind::Node {
            cur.decimicro_lat = coord_to_decimicro(e, "lat", id)?;
            cur.decimicro_lon = coord_to_decimicro(e, "lon", id)?;
        }
        self.current = Some(cur);
        Ok(())
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> Result<()> {
        if let Some(cur) = self.current.as_mut() {
            let k = parse_str_attr(e, "k")?;
            let v = parse_str_attr(e, "v")?;
            cur.tags.push((k, v));
        }
        Ok(())
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> Result<()> {
        let id = parse_i64_attr(e, "ref")?;
        if let Some(cur) = self.current.as_mut() {
            cur.refs.push(id);
        }
        Ok(())
    }

    fn member(&mut self, e: &BytesStart<'_>) -> Result<()> {
        let id = parse_i64_attr(e, "ref")?;
        // Deliberately normalize readable roles and replace malformed roles with
        // empty text. Rejecting them would break the accepted OSC input contract.
        let role = parse_str_attr_optional(e, "role").unwrap_or_default();
        // The shared `parse_member_type` refuses anything that is not
        // node/way/relation, so there is no `MemberId::Unknown` arm to
        // write here any more. This parser used to construct one - and
        // `osc::write::member_type_str` refuses to emit it, so a
        // relation with an exotic member type was accepted at parse and
        // then failed at write.
        let member_id = match parse_member_type(&parse_str_attr(e, "type")?)? {
            crate::MemberType::Node => MemberId::Node(id),
            crate::MemberType::Way => MemberId::Way(id),
            crate::MemberType::Relation => MemberId::Relation(id),
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
    if !matches_any(expressions, &cur.tags, cur.kind) {
        return;
    }

    match (section, cur.kind) {
        (Section::Create, ElementKind::Node) => {
            stats.creates_out += 1;
            filtered.create_nodes.push(OwnedNode {
                id: cur.id,
                decimicro_lat: cur.decimicro_lat,
                decimicro_lon: cur.decimicro_lon,
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
                decimicro_lat: cur.decimicro_lat,
                decimicro_lon: cur.decimicro_lon,
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

fn write_filtered_osc(output: &crate::output::Output, filtered: &FilteredOsc) -> Result<()> {
    // Deliberately gzip every OSC output regardless of its name. Inferring the
    // transport from an extension would change this command's output contract.
    let gz = GzEncoder::new(output.writer()?, flate2::Compression::fast());
    let mut writer = Writer::new_with_indent(gz, b' ', 2);

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

    if !filtered.delete_node_ids.is_empty()
        || !filtered.delete_way_ids.is_empty()
        || !filtered.delete_relation_ids.is_empty()
    {
        writer.write_event(Event::Start(BytesStart::new("delete")))?;
        for (id, meta) in &filtered.delete_node_ids {
            write_delete_xml(&mut writer, ElementKind::Node, *id, meta.as_ref())?;
        }
        for (id, meta) in &filtered.delete_way_ids {
            write_delete_xml(&mut writer, ElementKind::Way, *id, meta.as_ref())?;
        }
        for (id, meta) in &filtered.delete_relation_ids {
            write_delete_xml(&mut writer, ElementKind::Relation, *id, meta.as_ref())?;
        }
        writer.write_event(Event::End(BytesEnd::new("delete")))?;
    }

    writer.write_event(Event::End(BytesEnd::new("osmChange")))?;
    // Finish the gzip stream, then the output's writer, whose checked flush
    // is what lets the output be published.
    let gz = writer.into_inner();
    gz.finish()?.finish()
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
