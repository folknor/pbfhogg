//! Look up a single element by type + ID and print its metadata, tags, and
//! refs/members.

use std::io::{self, Write};
use std::path::Path;

use super::super::Result;
use crate::Element;
use crate::ElementKind;

/// Look up a single element by type and ID and write all its metadata,
/// tags and refs/members to `out`. Returns whether the element was found;
/// nothing is written when it was not. Uses blob-level indexdata when
/// available to skip non-matching blobs. On sorted PBFs, exits early once
/// past the target ID range.
pub fn show_element_to<W: Write>(
    path: &Path,
    target_kind: ElementKind,
    target_id: i64,
    direct_io: bool,
    out: &mut W,
    host: &crate::host::Host,
) -> Result<bool> {
    host.cancel
        .decided(|| show_element_inner(path, target_kind, target_id, direct_io, out, host))
}

fn show_element_inner<W: Write>(
    path: &Path,
    target_kind: ElementKind,
    target_id: i64,
    direct_io: bool,
    out: &mut W,
    host: &crate::host::Host,
) -> Result<bool> {
    let mut reader = crate::blob::BlobReader::open_in(path, direct_io, host)?;
    reader.set_parse_indexdata(true);
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    // The `min_id > target_id` early-exit below is only sound when the
    // PBF declares `Sort.Type_then_ID`; on an unsorted PBF a later
    // same-kind blob can have a smaller `min_id` than an earlier blob.
    // Decode the header blob once up front to establish the invariant.
    let mut is_sorted = false;

    let mut first = true;
    for blob_result in &mut reader {
        let blob = blob_result?;
        // The header comes first or the file is not a PBF: a zero-byte or
        // header-less file gets the one missing-header error, not "not found"
        // (or a hit in data blobs the header never described).
        if first && blob.get_type() != crate::blob::BlobType::OsmHeader {
            return Err(crate::error::new_missing_header(path));
        }
        first = false;
        match blob.get_type() {
            crate::blob::BlobType::OsmHeader => {
                let header = blob.to_headerblock()?;
                is_sorted = header.is_sorted();
                continue;
            }
            crate::blob::BlobType::OsmData => {}
            _ => continue,
        }

        // Skip blobs that cannot contain the target element.
        if let Some(idx) = blob.index() {
            // Wrong element type.
            if idx.kind != target_kind {
                continue;
            }
            // Target ID outside this blob's range.
            if target_id < idx.min_id || target_id > idx.max_id {
                // Sorted PBFs: once past the target, no later blob can
                // contain it. Unsorted PBFs: must keep scanning - a
                // later blob can carry a smaller `min_id`.
                if is_sorted && idx.min_id > target_id {
                    return Ok(false);
                }
                continue;
            }
        }

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
    if let Some(info) = dn.info()
        && info.version() != -1
    {
        writeln!(out, "  version: {}", info.version())?;
        let ts = info.milli_timestamp();
        if ts != 0 {
            writeln!(out, "  timestamp: {}", ts / 1000)?;
        }
        let cs = info.changeset();
        if cs != -1 && cs != 0 {
            writeln!(out, "  changeset: {cs}")?;
        }
        let uid = info.uid();
        if uid != 0 {
            writeln!(out, "  uid: {uid}")?;
        }
        if let Ok(user) = info.user()
            && !user.is_empty()
        {
            writeln!(out, "  user: {user}")?;
        }
    }
    Ok(())
}

fn write_info(out: &mut impl Write, info: &crate::Info<'_>) -> io::Result<()> {
    if let Some(v) = info.version() {
        writeln!(out, "  version: {v}")?;
    }
    if let Some(ts) = info.milli_timestamp()
        && ts != 0
    {
        writeln!(out, "  timestamp: {}", ts / 1000)?;
    }
    if let Some(cs) = info.changeset()
        && cs != 0
    {
        writeln!(out, "  changeset: {cs}")?;
    }
    if let Some(uid) = info.uid()
        && uid != 0
    {
        writeln!(out, "  uid: {uid}")?;
    }
    if let Some(Ok(user)) = info.user()
        && !user.is_empty()
    {
        writeln!(out, "  user: {user}")?;
    }
    Ok(())
}

/// Write tags for elements that implement the standard `tags()` iterator.
fn write_tags<'a>(out: &mut impl Write, element: &impl HasTags<'a>) -> io::Result<()> {
    let mut has_tags = false;
    for (k, v) in element.tags() {
        if !has_tags {
            writeln!(out, "  tags:")?;
            has_tags = true;
        }
        writeln!(out, "    {k} = {v}")?;
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
        writeln!(out, "    {k} = {v}")?;
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
            let type_str = match m.id {
                crate::MemberId::Node(_) => "node",
                crate::MemberId::Way(_) => "way",
                crate::MemberId::Relation(_) => "relation",
                crate::MemberId::Unknown(_, _) => "unknown",
            };
            let role = m.role().unwrap_or("<invalid>");
            writeln!(out, "    {type_str}/{} ({})", m.id.id(), role)?;
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
