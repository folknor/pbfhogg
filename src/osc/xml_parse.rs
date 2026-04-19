//! Quick-xml state machine that drives `parse_osc_file_into`. Translates
//! OSC (.osc) XML events into arena appends on a [`CompactDiffOverlay`].

use crate::read::elements::MemberType;

use super::compact::member_type_to_byte;
use super::parse::CompactDiffOverlay;
use super::ParseResult;

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
            let val = attr.unescape_value()?;
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
    }
}

/// Finalize the current element: build the tag slice, append to the appropriate
/// arena, insert into the index, and clear staging.
fn finalize_element(state: &mut ParserState, overlay: &mut CompactDiffOverlay) {
    let tags: Vec<(u32, &str)> = state
        .tag_keys
        .iter()
        .zip(state.tag_values.iter())
        .map(|(&k, v)| (k, v.as_str()))
        .collect();

    match state.current_elem {
        CurrentElem::Node => {
            overlay.push_node(state.current_id, state.current_lat, state.current_lon, &tags);
        }
        CurrentElem::Way => {
            overlay.push_way(state.current_id, &state.refs, &tags);
        }
        CurrentElem::Relation => {
            overlay.push_relation(state.current_id, &state.members, &tags);
        }
        CurrentElem::None => {}
    }

    state.clear_staging();
}

/// Handle the opening tag (or self-closing tag) for a node/way/relation element.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
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

    if elem_kind == CurrentElem::Node {
        let lat = parse_f64_attr(e, b"lat").unwrap_or(0.0);
        let lon = parse_f64_attr(e, b"lon").unwrap_or(0.0);
        state.current_lat = (lat * 1e7).round() as i32;
        state.current_lon = (lon * 1e7).round() as i32;
    }

    if is_empty {
        // Self-closing element: immediately finalize with empty tags/refs/members.
        state.current_elem = elem_kind;
        finalize_element(state, overlay);
    } else {
        state.current_elem = elem_kind;
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
    let key_id = overlay.intern(&k);
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
    let type_byte = member_type_to_byte(member_type);
    let role_id = overlay.intern(&role);
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
) {
    match e.name().as_ref() {
        b"create" | b"modify" | b"delete" => state.section = Section::None,
        b"node" | b"way" | b"relation"
            if state.current_elem != CurrentElem::None =>
        {
            finalize_element(state, overlay);
        }
        _ => {}
    }
}
