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
//! a created or modified node (`coord_to_decimicro` then `format_coord`),
//! then the metadata in `OwnedMetadata::push_attrs` order and escaping; a
//! deleted object is `id` and metadata only; an element with no retained
//! child is self-closing. The differential tests hold the sink to the owned
//! path byte for byte.

use quick_xml::events::{BytesEnd, BytesStart, Event};

use super::{Action, OscWriter, action_tag};
use crate::ElementKind;
use crate::coord_fmt::{format_coord, from_decimicro};
use crate::osc::write::{OwnedMetadata, append_attr, append_attr_escaped, append_int_attr};
use crate::osc::xml_parse::{
    OscSink, append_str_attr, coord_to_decimicro, parse_i64_attr, parse_member_into, parse_nd,
    scan_owned_metadata_into,
};

/// Writes a batch's changes straight into `writer`, framing the action
/// sections as `emit_change` does.
pub(super) struct ByteSink<'w> {
    writer: &'w mut OscWriter,
    open_action: &'w mut Option<Action>,
    count: &'w mut u64,
    /// The current element: its action and kind, when one is open.
    current: Option<(Action, ElementKind)>,
    head: String,
    structural: String,
    structural_ranges: Vec<(usize, usize)>,
    tags: String,
    tag_ranges: Vec<(usize, usize)>,
    meta: OwnedMetadata,
    scratch: String,
    coord: String,
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
            current: None,
            head: String::new(),
            structural: String::new(),
            structural_ranges: Vec::new(),
            tags: String::new(),
            tag_ranges: Vec::new(),
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

    fn open(&mut self) -> crate::Result<ElementKind> {
        self.current.map(|(_, kind)| kind).ok_or_else(|| {
            crate::Error::internal("merge-changes: an object child with no current object")
        })
    }
}

impl OscSink for ByteSink<'_> {
    const END_ELEMENT_EMITS: bool = true;

    fn begin_element(
        &mut self,
        action: Action,
        kind: ElementKind,
        e: &BytesStart<'_>,
    ) -> crate::Result<()> {
        // The owned sink's order: id, metadata, then a live node's lat and
        // lon.
        let id = parse_i64_attr(e, "id")?;
        let has_meta = scan_owned_metadata_into(e, &mut self.meta)?;
        let coords = if kind == ElementKind::Node && action != Action::Delete {
            Some((
                coord_to_decimicro(e, "lat", id)?,
                coord_to_decimicro(e, "lon", id)?,
            ))
        } else {
            None
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
        self.current = Some((action, kind));
        Ok(())
    }

    fn tag(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.open()?;
        let start = self.tags.len();
        self.tags.push_str("tag");
        // k then v, as `parse_tag` reads them.
        self.scratch.clear();
        append_str_attr(e, "k", &mut self.scratch)?;
        append_attr_escaped(&mut self.tags, "k", &self.scratch);
        self.scratch.clear();
        append_str_attr(e, "v", &mut self.scratch)?;
        append_attr_escaped(&mut self.tags, "v", &self.scratch);
        self.tag_ranges.push((start, self.tags.len()));
        Ok(())
    }

    fn nd(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
        self.open()?;
        let ref_id = parse_nd(e)?;
        let start = self.structural.len();
        self.structural.push_str("nd");
        append_int_attr(&mut self.structural, "ref", ref_id);
        self.structural_ranges.push((start, self.structural.len()));
        Ok(())
    }

    fn member(&mut self, e: &BytesStart<'_>) -> crate::Result<()> {
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

    fn end_element(&mut self) -> crate::Result<()> {
        let Some((action, kind)) = self.current.take() else {
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
        let name = kind.as_str();
        let head = BytesStart::from_content(self.head.as_str(), name.len());
        if self.structural_ranges.is_empty() && self.tag_ranges.is_empty() {
            self.writer.write_event(Event::Empty(head))?;
        } else {
            self.writer.write_event(Event::Start(head))?;
            for &(start, end) in &self.structural_ranges {
                let content = &self.structural[start..end];
                let child_len = if content.starts_with("nd") { 2 } else { 6 };
                self.writer
                    .write_event(Event::Empty(BytesStart::from_content(content, child_len)))?;
            }
            for &(start, end) in &self.tag_ranges {
                self.writer
                    .write_event(Event::Empty(BytesStart::from_content(
                        &self.tags[start..end],
                        3,
                    )))?;
            }
            self.writer.write_event(Event::End(BytesEnd::new(name)))?;
        }
        *self.count += 1;
        Ok(())
    }
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
                &crate::host::Host::entered_for_tests().invocation,
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
  <node id="3" version="4" lat="bad"/>
  <way id="12" visible="false"><nd ref="1"/><tag k="k" v="v"/></way>
  <relation id="22"/>
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
        assert_eq!(changes, 9, "every element of the document was written");
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
