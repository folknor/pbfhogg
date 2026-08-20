//! Quick-xml state machine that drives `parse_osc_file_into`. Translates
//! OSC (.osc) XML events into arena appends on a [`CompactDiffOverlay`].

use crate::read::elements::MemberType;

use super::ParseResult;
use super::compact::{
    ArenaMeta, META_FLAG_HAS_USER, META_FLAG_HIDDEN, META_FLAG_PRESENT, member_type_to_byte,
};
use super::parse::CompactDiffOverlay;

// ---------------------------------------------------------------------------
// Section tracking
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Create,
    Modify,
    Delete,
}

// ---------------------------------------------------------------------------
// Attribute parsing helpers
// ---------------------------------------------------------------------------

fn parse_i64_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<i64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = std::str::from_utf8(&attr.value)?;
            let parsed = val.parse::<i64>()?;
            return Ok(parsed);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_f64_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<f64> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = std::str::from_utf8(&attr.value)?;
            let parsed = val.parse::<f64>()?;
            return Ok(parsed);
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

fn parse_str_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> ParseResult<String> {
    for attr_result in e.attributes() {
        let attr = attr_result?;
        if attr.key.as_ref() == name {
            let val = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
            return Ok(val.into_owned());
        }
    }
    Err(format!("missing attribute '{}'", String::from_utf8_lossy(name)).into())
}

// ---------------------------------------------------------------------------
// Parser staging
// ---------------------------------------------------------------------------

/// Which element type is currently being parsed (between start and end tags).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentElem {
    None,
    Node,
    Way,
    Relation,
}

// ---------------------------------------------------------------------------
// Parser handler functions
// ---------------------------------------------------------------------------

/// Convert an OSC XML member type string ("node", "way", "relation") to
/// the crate's `MemberType` enum.
fn parse_member_type(s: &str) -> ParseResult<MemberType> {
    match s {
        "node" => Ok(MemberType::Node),
        "way" => Ok(MemberType::Way),
        "relation" => Ok(MemberType::Relation),
        other => Err(format!("unknown relation member type: '{other}'").into()),
    }
}

/// State carried through the parser loop. Extracted into a struct to keep
/// handler function signatures from exceeding the too_many_arguments lint.
pub(super) struct ParserState {
    section: Section,
    current_elem: CurrentElem,
    current_id: i64,
    current_lat: i32,
    current_lon: i32,
    current_meta: ArenaMeta,
    tag_keys: Vec<u32>,
    tag_values: Vec<String>,
    refs: Vec<i64>,
    members: Vec<(i64, u8, u32)>,
}

impl ParserState {
    pub(super) fn new() -> Self {
        Self {
            section: Section::None,
            current_elem: CurrentElem::None,
            current_id: 0,
            current_lat: 0,
            current_lon: 0,
            current_meta: ArenaMeta::default(),
            tag_keys: Vec::new(),
            tag_values: Vec::new(),
            refs: Vec::new(),
            members: Vec::new(),
        }
    }

    fn clear_staging(&mut self) {
        self.tag_keys.clear();
        self.tag_values.clear();
        self.refs.clear();
        self.members.clear();
        self.current_elem = CurrentElem::None;
        self.current_meta = ArenaMeta::default();
    }
}

/// Parse the element metadata attributes (`version`, `timestamp`,
/// `changeset`, `uid`, `user`, `visible`) in one pass over the attribute
/// list. Lenient by design: any attribute that is absent or unparsable
/// leaves its field at the zero default (real OSC producers always emit
/// well-formed values; being strict here would make the whole parse fail
/// on an exotic producer's metadata quirk while the element data itself
/// is fine). The present flag is set when ANY metadata attribute was
/// seen, mirroring `merge_changes::parse_metadata`'s leniency.
fn parse_meta_attrs(
    e: &quick_xml::events::BytesStart,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<ArenaMeta> {
    let mut meta = ArenaMeta::default();
    for attr_result in e.attributes() {
        let attr = attr_result?;
        match attr.key.as_ref() {
            b"version" => {
                let val = std::str::from_utf8(&attr.value)?;
                meta.version = val.parse::<i32>().unwrap_or(0);
                meta.flags |= META_FLAG_PRESENT;
            }
            b"timestamp" => {
                let val = std::str::from_utf8(&attr.value)?;
                meta.timestamp = crate::commands::parse_rfc3339_utc(val).unwrap_or(0);
                meta.flags |= META_FLAG_PRESENT;
            }
            b"changeset" => {
                let val = std::str::from_utf8(&attr.value)?;
                meta.changeset = val.parse::<i64>().unwrap_or(0);
                meta.flags |= META_FLAG_PRESENT;
            }
            b"uid" => {
                let val = std::str::from_utf8(&attr.value)?;
                meta.uid = val.parse::<i32>().unwrap_or(0);
                meta.flags |= META_FLAG_PRESENT;
            }
            b"user" => {
                let val = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?;
                meta.user_id = overlay.intern(&val)?;
                meta.flags |= META_FLAG_PRESENT | META_FLAG_HAS_USER;
            }
            b"visible" => {
                if attr.value.as_ref() == b"false" {
                    meta.flags |= META_FLAG_HIDDEN;
                }
                meta.flags |= META_FLAG_PRESENT;
            }
            _ => {}
        }
    }
    Ok(meta)
}

/// Finalize the current element: build the tag slice, append to the appropriate
/// arena, insert into the index, and clear staging.
fn finalize_element(state: &mut ParserState, overlay: &mut CompactDiffOverlay) -> ParseResult<()> {
    let tags: Vec<(u32, &str)> = state
        .tag_keys
        .iter()
        .zip(state.tag_values.iter())
        .map(|(&k, v)| (k, v.as_str()))
        .collect();

    let result = match state.current_elem {
        CurrentElem::Node => overlay.push_node(
            state.current_id,
            state.current_lat,
            state.current_lon,
            &tags,
            &state.current_meta,
        ),
        CurrentElem::Way => {
            overlay.push_way(state.current_id, &state.refs, &tags, &state.current_meta)
        }
        CurrentElem::Relation => {
            overlay.push_relation(state.current_id, &state.members, &tags, &state.current_meta)
        }
        CurrentElem::None => Ok(()),
    };

    state.clear_staging();
    result
}

/// Convert one OSC coordinate attribute to decimicrodegrees.
///
/// Geometry is not metadata, so the leniency `parse_meta_attrs` documents
/// does not extend here. A create/modify `<node>` whose `lat`/`lon` is
/// missing, empty or unparsable used to land at exactly (0, 0) - and 0/0
/// is a real coordinate in the Gulf of Guinea, so the result was
/// indistinguishable from a genuine node there. Worse, under
/// `--locations-on-ways` that (0, 0) propagates into every way referencing
/// the node. Out-of-range and non-finite values were equally silent: `as
/// i32` saturates `lat="1e30"` to `i32::MAX` and maps `NaN` to 0.
fn coord_to_decimicro(e: &quick_xml::events::BytesStart, name: &[u8], id: i64) -> ParseResult<i32> {
    let degrees = parse_f64_attr(e, name).map_err(|err| -> Box<dyn std::error::Error> {
        format!(
            "OSC node {id}: {} attribute is missing or unparsable ({err}). A created or \
             modified node must carry usable coordinates; storing it at 0/0 would silently \
             move it to the Gulf of Guinea.",
            String::from_utf8_lossy(name)
        )
        .into()
    })?;
    // Two independent checks. The geographic one comes first because it is
    // the one a caller can reason about: lat="100" is not a place, and an
    // i32 happily holds 1_000_000_000 decimicrodegrees, so the
    // representation check below would wave it through. Such a node would
    // enter the overlay as an ordinary node and could propagate into
    // LocationsOnWays, where nothing downstream re-validates it.
    let limit = if name == b"lat" { 90.0_f64 } else { 180.0_f64 };
    if !(degrees.is_finite() && degrees.abs() <= limit) {
        return Err(format!(
            "OSC node {id}: {} value {degrees} is outside the valid geographic range \
             -{limit} to {limit}",
            String::from_utf8_lossy(name)
        )
        .into());
    }
    let scaled = degrees * 1e7;
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(format!(
            "OSC node {id}: {} value {degrees} is outside the range representable in \
             decimicrodegrees",
            String::from_utf8_lossy(name)
        )
        .into());
    }
    // The bounds check above rules out every value `as i32` would saturate
    // or map to 0, so the cast is exact.
    #[allow(clippy::cast_possible_truncation)]
    Ok(scaled.round() as i32)
}

/// Handle the opening tag (or self-closing tag) for a node/way/relation element.
fn handle_elem_start(
    e: &quick_xml::events::BytesStart,
    elem_kind: CurrentElem,
    is_empty: bool,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    let id = parse_i64_attr(e, b"id")?;

    if state.section == Section::Delete {
        match elem_kind {
            CurrentElem::Node => overlay.delete_node(id),
            CurrentElem::Way => overlay.delete_way(id),
            CurrentElem::Relation => overlay.delete_relation(id),
            CurrentElem::None => {}
        }
        // For deletes, do not set current_elem (no child elements expected).
        return Ok(());
    }

    // Create/modify: remove from deleted sets if re-created.
    match elem_kind {
        CurrentElem::Node => {
            overlay.deleted_nodes.remove(&id);
        }
        CurrentElem::Way => {
            overlay.deleted_ways.remove(&id);
        }
        CurrentElem::Relation => {
            overlay.deleted_relations.remove(&id);
        }
        CurrentElem::None => {}
    }

    state.current_id = id;
    state.current_meta = parse_meta_attrs(e, overlay)?;

    if elem_kind == CurrentElem::Node {
        state.current_lat = coord_to_decimicro(e, b"lat", id)?;
        state.current_lon = coord_to_decimicro(e, b"lon", id)?;
    }

    state.current_elem = elem_kind;
    if is_empty {
        // Self-closing element: immediately finalize with empty tags/refs/members.
        finalize_element(state, overlay)?;
    }

    Ok(())
}

/// Handle a `<tag k="..." v="..."/>` element.
fn handle_tag_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    if state.current_elem == CurrentElem::None {
        return Ok(());
    }
    let k = parse_str_attr(e, b"k")?;
    let v = parse_str_attr(e, b"v")?;
    let key_id = overlay.intern(&k)?;
    state.tag_keys.push(key_id);
    state.tag_values.push(v);
    Ok(())
}

/// Handle a `<nd ref="..."/>` element.
fn handle_nd_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
) -> ParseResult<()> {
    if state.current_elem != CurrentElem::Way {
        return Ok(());
    }
    let ref_id = parse_i64_attr(e, b"ref")?;
    state.refs.push(ref_id);
    Ok(())
}

/// Handle a `<member type="..." ref="..." role="..."/>` element.
fn handle_member_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    if state.current_elem != CurrentElem::Relation {
        return Ok(());
    }
    let member_type_str = parse_str_attr(e, b"type")?;
    let member_type = parse_member_type(&member_type_str)?;
    let ref_id = parse_i64_attr(e, b"ref")?;
    let role = parse_str_attr(e, b"role").unwrap_or_default();
    let type_byte = member_type_to_byte(member_type)?;
    let role_id = overlay.intern(&role)?;
    state.members.push((ref_id, type_byte, role_id));
    Ok(())
}

/// Dispatch a Start event to the appropriate handler.
pub(super) fn handle_start_event_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"create" => state.section = Section::Create,
        b"modify" => state.section = Section::Modify,
        b"delete" => state.section = Section::Delete,
        b"node" => handle_elem_start(e, CurrentElem::Node, false, state, overlay)?,
        b"way" => handle_elem_start(e, CurrentElem::Way, false, state, overlay)?,
        b"relation" => handle_elem_start(e, CurrentElem::Relation, false, state, overlay)?,
        b"tag" => handle_tag_compact(e, state, overlay)?,
        b"nd" => handle_nd_compact(e, state)?,
        b"member" => handle_member_compact(e, state, overlay)?,
        _ => {}
    }
    Ok(())
}

/// Dispatch an Empty (self-closing) event to the appropriate handler.
pub(super) fn handle_empty_event_compact(
    e: &quick_xml::events::BytesStart,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"node" => handle_elem_start(e, CurrentElem::Node, true, state, overlay)?,
        b"way" => handle_elem_start(e, CurrentElem::Way, true, state, overlay)?,
        b"relation" => handle_elem_start(e, CurrentElem::Relation, true, state, overlay)?,
        b"tag" => handle_tag_compact(e, state, overlay)?,
        b"nd" => handle_nd_compact(e, state)?,
        b"member" => handle_member_compact(e, state, overlay)?,
        _ => {}
    }
    Ok(())
}

/// Dispatch an End event to the appropriate handler.
pub(super) fn handle_end_event_compact(
    e: &quick_xml::events::BytesEnd,
    state: &mut ParserState,
    overlay: &mut CompactDiffOverlay,
) -> ParseResult<()> {
    match e.name().as_ref() {
        b"create" | b"modify" | b"delete" => state.section = Section::None,
        b"node" | b"way" | b"relation" if state.current_elem != CurrentElem::None => {
            finalize_element(state, overlay)?;
        }
        _ => {}
    }
    Ok(())
}
