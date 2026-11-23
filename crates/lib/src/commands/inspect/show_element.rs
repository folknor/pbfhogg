//! Look up a single element by type + ID and print its metadata, tags, and
//! refs/members.

use std::io::{self, Write};
use std::path::Path;

use super::super::Result;
use crate::Element;
use crate::ElementKind;
use crate::read::claims::MetaClaims;
use crate::text_quote::{MemberLabel, Quoted};

/// Look up a single element by type and ID and write all its metadata,
/// tags and refs/members to `out`. Returns whether the element was found;
/// nothing is written when it was not. Uses blob-level indexdata when
/// available to skip non-matching blobs without reading their payloads. A
/// miss walks the whole file: the header's sort claim is not used to stop
/// early, since it is believed rather than checked.
pub fn show_element_to<W: Write>(
    path: &Path,
    target_kind: ElementKind,
    target_id: i64,
    direct_io: bool,
    out: &mut W,
    host: &crate::host::Host,
) -> Result<bool> {
    let host = &host.enter()?;
    host.invocation
        .decided(|| show_element_inner(path, target_kind, target_id, direct_io, out, host))
}

fn show_element_inner<W: Write>(
    path: &Path,
    target_kind: ElementKind,
    target_id: i64,
    direct_io: bool,
    out: &mut W,
    host: &crate::host::Entered<'_>,
) -> Result<bool> {
    // An inspect description, exempt from the command-input refusal of a
    // partially indexed file: a blob without indexdata is decoded and
    // searched like the rest.
    let mut reader =
        crate::blob::BlobReader::open_in(path, direct_io, host)?.accepting_partial_index();
    reader.set_parse_indexdata(true);
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    // A header-first walk: each frame's indexdata (in its `BlobHeader`) can
    // rule that one blob out, and its payload is then skipped unread. Only
    // the end of the file rules the target out. There is deliberately no
    // early exit on a `Sort.Type_then_ID` header: the stamp is believed, not
    // checked, and a file whose blobs run out of order under it (honest
    // indexdata, node blobs `[2]` then `[1]`) would otherwise report a
    // present element absent. A miss costs a header walk, plus a decode of
    // every blob whose index cannot exclude the target. A skipped body's
    // `Blob` envelope is never parsed, so a malformed one there is not
    // refused; a truncated one still is, by the frame walk.
    let mut first = true;
    while let Some(frame) = reader.next_frame_header()? {
        let is_header = matches!(frame.header.blob_type, crate::blob::BlobKind::OsmHeader);
        let is_data = matches!(frame.header.blob_type, crate::blob::BlobKind::OsmData);
        // The header comes first or the file is not a PBF: a zero-byte or
        // header-less file gets the one missing-header error, not "not found"
        // (or a hit in data blobs the header never described).
        if first && !is_header {
            return Err(crate::error::new_missing_header(path));
        }
        if std::mem::replace(&mut first, false) {
            // The leading header is decoded, so a malformed one is refused.
            reader.read_frame_blob(frame)?.to_headerblock()?;
            continue;
        }
        if !is_data {
            reader.skip_frame_payload()?;
            continue;
        }

        // Skip blobs that cannot contain the target element: wrong element
        // type, or the target outside the claimed range (a signed numeric
        // compare, exact for IDs of either sign).
        let index = frame
            .header
            .indexdata
            .as_ref()
            .and_then(|d| crate::blob_meta::BlobIndex::deserialize(d));
        if let Some(idx) = index
            && (idx.kind != target_kind || target_id < idx.min_id || target_id > idx.max_id)
        {
            reader.skip_frame_payload()?;
            continue;
        }

        let blob = reader.read_frame_blob(frame)?;

        blob.decompress_into(&mut decompress_buf)?;
        let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
            std::mem::take(&mut decompress_buf),
            &mut st_scratch,
            &mut gr_scratch,
        )?;

        for element in block.elements() {
            match (&element, target_kind) {
                (Element::DenseNode(dn), ElementKind::Node) if dn.id() == target_id => {
                    write_node_header(out, target_id, dn.lat(), dn.lon())?;
                    write_dense_node_info(out, dn)?;
                    write_tags_dense(out, dn)?;
                    out.flush()?;
                    return Ok(true);
                }
                (Element::Node(n), ElementKind::Node) if n.id() == target_id => {
                    write_node_header(out, target_id, n.lat(), n.lon())?;
                    write_info(out, &n.info())?;
                    write_tags(out, n)?;
                    out.flush()?;
                    return Ok(true);
                }
                (Element::Way(w), ElementKind::Way) if w.id() == target_id => {
                    writeln!(out, "way/{target_id}")?;
                    write_info(out, &w.info())?;
                    write_tags(out, w)?;
                    write_way_refs(out, w)?;
                    out.flush()?;
                    return Ok(true);
                }
                (Element::Relation(r), ElementKind::Relation) if r.id() == target_id => {
                    writeln!(out, "relation/{target_id}")?;
                    write_info(out, &r.info())?;
                    write_tags(out, r)?;
                    write_relation_members(out, r)?;
                    out.flush()?;
                    return Ok(true);
                }
                _ => {}
            }
        }
    }

    if first {
        return Err(crate::error::new_missing_header(path));
    }
    Ok(false)
}

fn write_node_header(out: &mut impl Write, id: i64, lat: f64, lon: f64) -> io::Result<()> {
    writeln!(out, "node/{id}")?;
    writeln!(out, "  lat: {lat:.7}")?;
    writeln!(out, "  lon: {lon:.7}")
}

fn write_dense_node_info(out: &mut impl Write, dn: &crate::DenseNode<'_>) -> io::Result<()> {
    // `claims` reads presence from the slot's values: a version of 0 or -1
    // is not a claim, and the padding slot of a node without metadata in a
    // block whose other nodes carry it claims nothing.
    if let Some(info) = dn.info()
        && let Some(claims) = MetaClaims::from_dense(info)
    {
        write_meta(out, &claims)?;
    }
    Ok(())
}

fn write_info(out: &mut impl Write, info: &crate::Info<'_>) -> io::Result<()> {
    write_meta(out, &MetaClaims::from_info(info))
}

/// Every claimed field as claimed, zeros and an empty user included, and
/// nothing for a field the element does not claim, so an absent field and a
/// present zero print differently. The timestamp prints in whole epoch
/// seconds, floored like export and OSC. `visible` prints whenever claimed:
/// a deleted history object shows `visible: false`; a plain element without
/// the field prints no line.
fn write_meta(out: &mut impl Write, claims: &MetaClaims<'_>) -> io::Result<()> {
    if let Some(v) = claims.version {
        writeln!(out, "  version: {v}")?;
    }
    match &claims.milli_timestamp {
        // A scaling that overflows i64 milliseconds prints the marker, like
        // a bad user index, rather than the wrapped value.
        Some(Ok(ts)) => writeln!(out, "  timestamp: {}", ts.div_euclid(1000))?,
        Some(Err(_)) => writeln!(out, "  timestamp: <invalid>")?,
        None => {}
    }
    if let Some(cs) = claims.changeset {
        writeln!(out, "  changeset: {cs}")?;
    }
    if let Some(uid) = claims.uid {
        writeln!(out, "  uid: {uid}")?;
    }
    if let Some(user) = &claims.user {
        write_user(out, user.as_ref().ok().copied())?; // ok-deliberate: a bad index prints the invalid marker
    }
    if let Some(visible) = claims.visible {
        writeln!(out, "  visible: {visible}")?;
    }
    Ok(())
}

/// `  user: "name"` (an empty name as `""`), and a bare `<invalid>` for a
/// negative or out-of-range user string index, passed as `None` (a real name
/// is always quoted, so the sentinel cannot be mistaken for one).
fn write_user(out: &mut impl Write, user: Option<&str>) -> io::Result<()> {
    match user {
        Some(user) => writeln!(out, "  user: {}", Quoted(user)),
        None => writeln!(out, "  user: <invalid>"),
    }
}

/// Write tags for elements that implement the standard `tags()` iterator.
fn write_tags<'a>(out: &mut impl Write, element: &impl HasTags<'a>) -> io::Result<()> {
    let mut has_tags = false;
    for (k, v) in element.tags() {
        if !has_tags {
            writeln!(out, "  tags:")?;
            has_tags = true;
        }
        writeln!(out, "    {}={}", Quoted(k), Quoted(v))?;
    }
    Ok(())
}

fn write_tags_dense(out: &mut impl Write, dn: &crate::DenseNode<'_>) -> io::Result<()> {
    let mut has_tags = false;
    for (k, v) in dn.tags() {
        if !has_tags {
            writeln!(out, "  tags:")?;
            has_tags = true;
        }
        writeln!(out, "    {}={}", Quoted(k), Quoted(v))?;
    }
    Ok(())
}

fn write_way_refs(out: &mut impl Write, w: &crate::Way<'_>) -> io::Result<()> {
    let refs: Vec<i64> = w.refs().collect();
    if !refs.is_empty() {
        writeln!(out, "  refs: ({} nodes)", refs.len())?;
        for id in &refs {
            writeln!(out, "    {id}")?;
        }
    }
    Ok(())
}

fn write_relation_members(out: &mut impl Write, r: &crate::Relation<'_>) -> io::Result<()> {
    let members: Vec<_> = r.members().collect();
    if !members.is_empty() {
        writeln!(out, "  members: ({})", members.len())?;
        for m in &members {
            // A real role is always quoted, so the bare `<invalid>` sentinel
            // for a negative or out-of-range string index is unambiguous.
            match m.role() {
                Ok(role) => writeln!(out, "    {} {}", MemberLabel(m.id), Quoted(role))?,
                Err(_) => writeln!(out, "    {} <invalid>", MemberLabel(m.id))?,
            }
        }
    }
    Ok(())
}

/// Trait to abstract over `Node`/`Way`/`Relation` tag access.
trait HasTags<'a> {
    fn tags(&self) -> crate::TagIter<'a>;
}

impl<'a> HasTags<'a> for crate::Node<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}

impl<'a> HasTags<'a> for crate::Way<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}

impl<'a> HasTags<'a> for crate::Relation<'a> {
    fn tags(&self) -> crate::TagIter<'a> {
        self.tags()
    }
}
