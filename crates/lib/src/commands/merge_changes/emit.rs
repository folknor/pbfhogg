//! The streaming merge's worker sink: a batch's recorded callbacks checked
//! by the same parsers [`super::OwnedSink`] runs, in the same order, and
//! written as the same XML bytes the owned writers produce, without
//! building an owned element.
//!
//! Per element it keeps three reused buffers: the start tag's content, the
//! structural children (`nd` / `member`) and the tags, each child as a
//! range of start tag content. The owned writers put every structural child
//! before every tag whatever their source order, so the children are kept
//! apart and written at the element's end, structural first; validation
//! still runs in callback order, so the first refusal is the one the owned
//! sink would report. Output is written only at the end of an element,
//! where the driver attributes a failure to the output, not the input.
//!
//! The byte contract is the owned writers': `id`, then `lat` and `lon` for
//! a node that has them (`coord_to_decimicro` then `format_coord`; always a
//! created or modified node, a deleted one when its input gave both, see
//! `delete_coords`), then the metadata in `OwnedMetadata::push_attrs` order
//! and escaping; a deleted object keeps the body its input gave, written
//! as a live one's; an element with no retained child is self-closing. The differential tests hold the sink to the owned
//! path byte for byte.

use quick_xml::events::{BytesEnd, BytesStart, Event};

use super::{Action, OscWriter, action_tag};
use crate::ElementKind;
use crate::coord_fmt::{format_coord, from_decimicro};
use crate::osc::write::{OwnedMetadata, append_attr, append_attr_escaped, append_int_attr};
use crate::osc::xml_parse::{
    DeleteChildren, OscSink, append_str_attr, coord_to_decimicro, delete_coords, parse_i64_attr,
    parse_member_into, parse_nd, scan_owned_metadata_into,
};

/// One element's start tag and children, checked and rendered into reused
/// buffers: the part every re-emitting OSC byte sink shares.
pub(crate) struct ElementBuf {
    owner: &'static str,
    id: i64,
    /// The current element: its action and kind, when one is open.
    current: Option<(Action, ElementKind)>,
    head: String,
    structural: String,
    structural_ranges: Vec<(usize, usize)>,
    tags: String,
    tag_ranges: Vec<(usize, usize)>,
    /// Each tag's key and value as read (unescaped), for selection.
    tag_text: String,
    tag_pairs: Vec<(usize, usize, usize)>,
    meta: OwnedMetadata,
    scratch: String,
    coord: String,
}

impl ElementBuf {
    /// `owner` names the command in the internal error for a child with no
    /// open object.
    pub(crate) fn new(owner: &'static str) -> Self {
        Self {
            owner,
            id: 0,
            current: None,
            head: String::new(),
            structural: String::new(),
            structural_ranges: Vec::new(),
            tags: String::new(),
            tag_ranges: Vec::new(),
            tag_text: String::new(),
            tag_pairs: Vec::new(),
            meta: OwnedMetadata {
                version: None,
                timestamp: String::new(),
                changeset: String::new(),
                uid: String::new(),
                user: String::new(),
                visible: String::new(),
            },
            scratch: String::new(),
            coord: String::new(),
        }
    }

    fn open(&self) -> crate::Result<ElementKind> {
        self.current.map(|(_, kind)| kind).ok_or_else(|| {
            crate::Error::internal(format!(
                "{}: an object child with no current object",
                self.owner
            ))
        })
    }

    pub(crate) fn id(&self) -> i64 {
        self.id
    }

    /// The element's `version` claim, `None` when it carried none.
    pub(crate) fn version(&self) -> Option<i32> {
        self.meta.version
    }

    /// The element's tags as read, key then value.
    pub(crate) fn tag_pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.tag_pairs
            .iter()
            .map(|&(k, v, e)| (&self.tag_text[k..v], &self.tag_text[v..e]))
    }

    /// End the open element, returning its action and kind; its buffers
    /// stay readable until the next `begin`.
    pub(crate) fn finish(&mut self) -> Option<(Action, ElementKind)> {
        self.current.take()
    }

    /// Write the element just finished as the owned writers write it.
    pub(crate) fn write<W: std::io::Write>(
        &self,
        writer: &mut quick_xml::Writer<W>,
        kind: ElementKind,
    ) -> crate::Result<()> {
        let name = kind.as_str();
        let head = BytesStart::from_content(self.head.as_str(), name.len());
        if self.structural_ranges.is_empty() && self.tag_ranges.is_empty() {
            writer.write_event(Event::Empty(head))?;
            return Ok(());
        }
        writer.write_event(Event::Start(head))?;
        for &(start, end) in &self.structural_ranges {
            let content = &self.structural[start..end];
            let child_len = if content.starts_with("nd") { 2 } else { 6 };
            writer.write_event(Event::Empty(BytesStart::from_content(content, child_len)))?;
        }
        for &(start, end) in &self.tag_ranges {
            writer.write_event(Event::Empty(BytesStart::from_content(
                &self.tags[start..end],
                3,
            )))?;
        }
        writer.write_event(Event::End(BytesEnd::new(name)))?;
        Ok(())
    }

    pub(crate) fn begin(
        &mut self,
        action: Action,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> crate::Result<()> {
        // The owned sink's order: id, metadata, then a node's lat and lon
        // (required of a live node, optional on a delete).
        let id = parse_i64_attr(e, "id")?;
        let has_meta = scan_owned_metadata_into(e, &mut self.meta)?;
        let coords = match (kind, action) {
            (ElementKind::Node, Action::Delete) => delete_coords(e, id)?,
            (ElementKind::Node, _) => Some((
                coord_to_decimicro(e, "lat", id)?,
                coord_to_decimicro(e, "lon", id)?,
            )),
            _ => None,
        };

        self.head.clear();
        self.head.push_str(kind.as_str());
        append_int_attr(&mut self.head, "id", id);
        if let Some((lat, lon)) = coords {
            format_coord(&mut self.coord, from_decimicro(lat));
            append_attr(&mut self.head, "lat", &self.coord);
            format_coord(&mut self.coord, from_decimicro(lon));
            append_attr(&mut self.head, "lon", &self.coord);
        }
        if has_meta {
            self.meta.append_attrs(&mut self.head);
        }
        self.structural.clear();
        self.structural_ranges.clear();
        self.tags.clear();
        self.tag_ranges.clear();
        self.tag_text.clear();
        self.tag_pairs.clear();
        self.id = id;
        self.current = Some((action, kind));
        Ok(())
    }

    pub(crate) fn tag(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.open()?;
        let start = self.tags.len();
        self.tags.push_str("tag");
        // k then v, as `parse_tag` reads them.
        let k = self.tag_text.len();
        append_str_attr(e, "k", &mut self.tag_text)?;
        let v = self.tag_text.len();
        append_attr_escaped(&mut self.tags, "k", &self.tag_text[k..v]);
        append_str_attr(e, "v", &mut self.tag_text)?;
        let end = self.tag_text.len();
        append_attr_escaped(&mut self.tags, "v", &self.tag_text[v..end]);
        self.tag_ranges.push((start, self.tags.len()));
        self.tag_pairs.push((k, v, end));
        Ok(())
    }

    pub(crate) fn nd(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.open()?;
        let ref_id = parse_nd(e)?;
        let start = self.structural.len();
        self.structural.push_str("nd");
        append_int_attr(&mut self.structural, "ref", ref_id);
        self.structural_ranges.push((start, self.structural.len()));
        Ok(())
    }

    pub(crate) fn member(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.open()?;
        let mut type_scratch = std::mem::take(&mut self.coord);
        self.scratch.clear();
        let parsed = parse_member_into(e, &mut type_scratch, &mut self.scratch);
        self.coord = type_scratch;
        let (kind, ref_id) = parsed?;
        let start = self.structural.len();
        self.structural.push_str("member");
        append_attr(&mut self.structural, "type", kind.as_str());
        append_int_attr(&mut self.structural, "ref", ref_id);
        append_attr_escaped(&mut self.structural, "role", &self.scratch);
        self.structural_ranges.push((start, self.structural.len()));
        Ok(())
    }
}

/// Writes a batch's changes straight into `writer`, framing the action
/// sections as `emit_change` does.
pub(super) struct ByteSink<'w> {
    writer: &'w mut OscWriter,
    open_action: &'w mut Option<Action>,
    count: &'w mut u64,
    /// When set, every written element's identity, version claim and byte
    /// range in the writer's buffer (see [`Element`]).
    index: Option<&'w mut Vec<Element>>,
    element: ElementBuf,
}

impl<'w> ByteSink<'w> {
    pub(super) fn new(
        writer: &'w mut OscWriter,
        open_action: &'w mut Option<Action>,
        count: &'w mut u64,
    ) -> Self {
        Self {
            writer,
            open_action,
            count,
            index: None,
            element: ElementBuf::new("merge-changes"),
        }
    }

    /// A sink that also records each element it writes into `index`. Only
    /// for a plain (not gzip) writer, whose buffer the ranges point into.
    pub(super) fn with_index(
        writer: &'w mut OscWriter,
        open_action: &'w mut Option<Action>,
        count: &'w mut u64,
        index: &'w mut Vec<Element>,
    ) -> Self {
        let mut sink = Self::new(writer, open_action, count);
        sink.index = Some(index);
        sink
    }
}

impl OscSink for ByteSink<'_> {
    const DELETE_CHILDREN: DeleteChildren = DeleteChildren::Route;
    const END_ELEMENT_EMITS: bool = true;

    fn begin_element(
        &mut self,
        action: Action,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> crate::Result<()> {
        self.element.begin(action, kind, e)
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.element.tag(e)
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.element.nd(e)
    }

    fn member(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.element.member(e)
    }

    fn end_element(&mut self) -> crate::Result<()> {
        let Some((action, kind)) = self.element.finish() else {
            return Ok(());
        };
        if *self.open_action != Some(action) {
            if let Some(prev) = self.open_action.take() {
                self.writer
                    .write_event(Event::End(BytesEnd::new(action_tag(prev))))?;
            }
            self.writer
                .write_event(Event::Start(BytesStart::new(action_tag(action))))?;
            *self.open_action = Some(action);
        }
        // After the action section is open, so the range holds the element
        // alone, its leading line break and indent included.
        let start = self.writer.len();
        self.element.write(&mut self.writer.writer, kind)?;
        if let Some(index) = self.index.as_deref_mut() {
            index.push(Element {
                kind,
                action,
                id: self.element.id(),
                version: self.element.version(),
                start,
                end: self.writer.len(),
            });
        }
        *self.count += 1;
        Ok(())
    }
}

/// One written element of an indexed [`ByteSink`]: what the simplify dedupe
/// keys and gates on, and where its bytes are. Written after its action
/// section is open (indent depth 1, line break pending), an element's bytes
/// do not depend on what precedes them in the section, so they can be
/// spliced into any section of the same action.
#[derive(Clone, Copy)]
pub(super) struct Element {
    pub(super) kind: ElementKind,
    pub(super) action: Action,
    pub(super) id: i64,
    /// The `version` claim, `None` when the element carried none.
    pub(super) version: Option<i32>,
    /// Byte range in the batch buffer. Not narrowed: one element's escaped
    /// text alone can exceed 4 GiB, and the batch target cuts only between
    /// elements.
    pub(super) start: usize,
    pub(super) end: usize,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unwrap_in_result)]
mod tests {
    use super::ByteSink;
    use crate::commands::merge_changes::{Action, OscWriter, OwnedSink, WriterDest, action_tag};
    use crate::osc::xml_parse::{LexBatch, LexSink, parse_osc_path};
    use quick_xml::events::{BytesEnd, Event};

    /// The batches the streaming reader records for `xml`, or its refusal.
    fn batches(dir: &std::path::Path, xml: &str) -> crate::Result<Vec<LexBatch>> {
        let path = dir.join("in.osc");
        std::fs::write(&path, xml).unwrap();
        let mut out = Vec::new();
        {
            let mut sink = LexSink::new(
                64,
                |batch: LexBatch| {
                    out.push(batch);
                    Ok(())
                },
                || false,
            );
            parse_osc_path(
                &path,
                &mut sink,
                &crate::host::Host::for_tests().entered().invocation,
            )?;
            sink.finish()?;
        }
        Ok(out)
    }

    /// One batch replayed into one sink, plain XML: the text and change
    /// count, or the refusal's message (compared as text: the two sinks must
    /// refuse with the same words).
    fn encode(batch: &LexBatch, owned: bool) -> Result<(String, u64), String> {
        let path = std::path::Path::new("in.osc");
        let mut writer = OscWriter::new_buf(false);
        let mut open_action: Option<Action> = None;
        let mut count = 0u64;
        let replayed = if owned {
            let mut sink = OwnedSink::new(WriterDest {
                writer: &mut writer,
                open_action: &mut open_action,
                count: &mut count,
            });
            batch.replay(&mut sink, path)
        } else {
            let mut sink = ByteSink::new(&mut writer, &mut open_action, &mut count);
            batch.replay(&mut sink, path)
        };
        if let Err(e) = replayed {
            return Err(e.report());
        }
        if let Some(prev) = open_action {
            writer
                .write_event(Event::End(BytesEnd::new(action_tag(prev))))
                .unwrap();
        }
        let bytes = writer.into_inner().unwrap();
        Ok((String::from_utf8(bytes).unwrap(), count))
    }

    fn assert_same(dir: &std::path::Path, xml: &str, what: &str) {
        let Ok(batches) = batches(dir, xml) else {
            // The reader refused it; no worker sees it.
            return;
        };
        for batch in &batches {
            assert_eq!(encode(batch, false), encode(batch, true), "{what}");
        }
    }

    const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
<create>
  <node id="1" lat="55.1000000" lon="-0.0" version="1" timestamp="2026-01-01T00:00:00Z" changeset="9" uid="3" user="a &amp; b&#10;c" visible="true">
    <tag k="na&quot;me" v="x&#9;y"/>
    <tag k="lit" v="with
newline"/>
  </node>
  <node id="2" lat="1e-3" lon="179.99999995" unknown="ignored"/>
  <way id="10" user="" uid="0" version="0"><tag k="a" v="b"/><nd ref="1"/><tag k="c" v="d"/><nd ref="2"/><bounds minlat="0"/></way>
  <relation id="20" timestamp="t"><tag k="type" v="route"/><member type="way" ref="10" role="&lt;outer&gt;"/><member type="node" ref="1"/></relation>
</create>
<modify>
  <way id="11" changeset="5"/>
  <relation id="21"></relation>
</modify>
<delete>
  <node id="3" version="4" lat="55.5" lon="-0.0"><tag k="amenity" v="bench"/></node>
  <node id="4" version="2"/>
  <way id="12" visible="false"><tag k="k" v="v"/><nd ref="1"/><bounds minlat="0"/></way>
  <relation id="22"><member type="node" ref="3" role="a&amp;b"/></relation>
</delete>
</osmChange>
"#;

    /// Byte-identical output across the shapes the owned writers reorder,
    /// normalize or drop.
    #[test]
    fn writes_what_the_owned_path_writes() {
        let dir = crate::path_guard::tests::scratch();
        let recorded = batches(dir.path(), DOC).expect("the document reads");
        let mut changes = 0;
        for batch in &recorded {
            let (text, count) = encode(batch, false).expect("the byte sink accepts it");
            assert_eq!(Ok((text, count)), encode(batch, true));
            changes += count;
        }
        assert_eq!(changes, 10, "every element of the document was written");
    }

    /// A delete keeps the body its input gave, in both paths; asserted on
    /// the bytes, so two equally lossy writers cannot agree their way past
    /// it.
    #[test]
    fn a_delete_keeps_its_body() {
        let dir = crate::path_guard::tests::scratch();
        let recorded = batches(dir.path(), DOC).expect("the document reads");
        let text: String = recorded
            .iter()
            .map(|batch| encode(batch, false).expect("the byte sink accepts it").0)
            .collect();
        // Small batches: the deletes span several `<delete>` sections, all
        // after the last live change.
        let delete = &text[text.find("<delete>").expect("a delete section")..];
        for needle in [
            r#"<node id="3" lat="55.5" lon="0" version="4">"#,
            r#"<tag k="amenity" v="bench"/>"#,
            r#"<node id="4" version="2"/>"#,
            r#"<nd ref="1"/>"#,
            r#"<tag k="k" v="v"/>"#,
            r#"<member type="node" ref="3" role="a&amp;b"/>"#,
        ] {
            assert!(delete.contains(needle), "{needle} missing from:\n{delete}");
        }
    }

    /// A delete's location is both coordinates or neither: half of one,
    /// or an unusable one, is refused by both paths alike.
    #[test]
    fn refuses_an_unusable_delete_location() {
        let dir = crate::path_guard::tests::scratch();
        for case in [
            r#"<node id="1" lat="1"/>"#,
            r#"<node id="1" lon="1"/>"#,
            r#"<node id="1" lat="bad" lon="1"/>"#,
            r#"<node id="1" lat="" lon="1"/>"#,
            r#"<node id="1" lat="91" lon="1"/>"#,
        ] {
            let xml = format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\"><delete>{case}</delete></osmChange>"
            );
            let recorded = batches(dir.path(), &xml).expect("the reader frames it");
            for batch in &recorded {
                let byte = encode(batch, false);
                assert!(byte.is_err(), "{case} accepted: {byte:?}");
                assert_eq!(byte, encode(batch, true), "{case}");
            }
        }
    }

    /// The first refusal is the owned sink's when several attributes are
    /// bad, in the start tag and across children.
    #[test]
    fn refuses_what_the_owned_path_refuses() {
        let dir = crate::path_guard::tests::scratch();
        let cases = [
            r#"<node id="x" lat="bad" lon="1" version="v"/>"#,
            r#"<node id="1" lat="bad" lon="1" version="v"/>"#,
            r#"<node id="1" lat="91" lon="1"/>"#,
            r#"<way id="1"><tag v="v"/><nd ref="x"/></way>"#,
            r#"<way id="1"><nd ref="x"/><tag v="v"/></way>"#,
            r#"<relation id="1"><member type="changeset" ref="x" role="&bad;"/></relation>"#,
            r#"<relation id="1"><member type="way" role="r"/></relation>"#,
            r#"<relation id="1"><member type="way" ref="1" role="&bad;"/></relation>"#,
            r#"<node id="1" lat="1" lon="1" user="&bad;"/>"#,
        ];
        for case in cases {
            let xml = format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\"><modify>{case}</modify></osmChange>"
            );
            assert_same(dir.path(), &xml, case);
        }
    }

    /// Every single-byte change of the document: the same bytes or the
    /// same refusal.
    #[test]
    fn agrees_under_mutation() {
        let dir = crate::path_guard::tests::scratch();
        let bytes = DOC.as_bytes();
        for pos in 0..bytes.len() {
            for &value in b"\"&x0-<" {
                let mut mutated = bytes.to_vec();
                mutated[pos] = value;
                let Ok(xml) = String::from_utf8(mutated) else {
                    continue;
                };
                assert_same(dir.path(), &xml, &format!("byte {pos} = {}", value as char));
            }
        }
    }
}
