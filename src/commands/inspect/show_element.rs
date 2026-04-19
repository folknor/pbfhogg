//! Look up a single element by type + ID and print its metadata, tags, and
//! refs/members.

use std::path::Path;

use super::super::Result;
use crate::blob_meta::ElemKind;
use crate::Element;

/// Element type filter for `show_element`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ShowElementType {
    Node,
    Way,
    Relation,
}

/// Look up a single element by type and ID. Prints all metadata, tags,
/// refs/members to stdout. Uses blob-level indexdata when available to
/// skip non-matching blobs. On sorted PBFs, exits early once past the
/// target ID range.
pub fn show_element(
    path: &Path,
    elem_type: ShowElementType,
    target_id: i64,
    direct_io: bool,
) -> Result<bool> {
    let mut reader = crate::blob::BlobReader::open(path, direct_io)?;
    reader.set_parse_indexdata(true);
    let mut decompress_buf: Vec<u8> = Vec::new();
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

    let target_kind = match elem_type {
        ShowElementType::Node => ElemKind::Node,
        ShowElementType::Way => ElemKind::Way,
        ShowElementType::Relation => ElemKind::Relation,
    };

    for blob_result in &mut reader {
        let blob = blob_result?;
        if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
            continue;
        }

        // Skip blobs that cannot contain the target element.
        if let Some(idx) = blob.index() {
            // Wrong element type.
            if idx.kind != target_kind {
                continue;
            }
            // Target ID outside this blob's range.
            if target_id < idx.min_id || target_id > idx.max_id {
                // On sorted PBFs, if min_id > target_id we're past it.
                if idx.min_id > target_id {
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
            match (&element, elem_type) {
                (Element::DenseNode(dn), ShowElementType::Node) if dn.id() == target_id => {
                    print_node_header(target_id, dn.lat(), dn.lon());
                    print_dense_node_info(dn);
                    print_tags_dense(dn);
                    return Ok(true);
                }
                (Element::Node(n), ShowElementType::Node) if n.id() == target_id => {
                    print_node_header(target_id, n.lat(), n.lon());
                    print_node_info(n);
                    print_tags(n);
                    return Ok(true);
                }
                (Element::Way(w), ShowElementType::Way) if w.id() == target_id => {
                    println!("way/{target_id}");
                    print_info(&w.info());
                    print_tags(w);
                    print_way_refs(w);
                    return Ok(true);
                }
                (Element::Relation(r), ShowElementType::Relation) if r.id() == target_id => {
                    println!("relation/{target_id}");
                    print_info(&r.info());
                    print_tags(r);
                    print_relation_members(r);
                    return Ok(true);
                }
                _ => {}
            }
        }
    }

    Ok(false)
}

fn print_node_header(id: i64, lat: f64, lon: f64) {
    println!("node/{id}");
    println!("  lat: {lat:.7}");
    println!("  lon: {lon:.7}");
}

fn print_dense_node_info(dn: &crate::DenseNode<'_>) {
    if let Some(info) = dn.info() {
        if info.version() != -1 {
            println!("  version: {}", info.version());
            let ts = info.milli_timestamp();
            if ts != 0 {
                println!("  timestamp: {}", ts / 1000);
            }
            let cs = info.changeset();
            if cs != -1 && cs != 0 {
                println!("  changeset: {cs}");
            }
            let uid = info.uid();
            if uid != 0 {
                println!("  uid: {uid}");
            }
            if let Ok(user) = info.user() {
                if !user.is_empty() {
                    println!("  user: {user}");
                }
            }
        }
    }
}

fn print_node_info(n: &crate::Node<'_>) {
    print_info(&n.info());
}

fn print_info(info: &crate::Info<'_>) {
    if let Some(v) = info.version() {
        println!("  version: {v}");
    }
    if let Some(ts) = info.milli_timestamp() {
        if ts != 0 {
            println!("  timestamp: {}", ts / 1000);
        }
    }
    if let Some(cs) = info.changeset() {
        if cs != 0 {
            println!("  changeset: {cs}");
        }
    }
    if let Some(uid) = info.uid() {
        if uid != 0 {
            println!("  uid: {uid}");
        }
    }
    if let Some(Ok(user)) = info.user() {
        if !user.is_empty() {
            println!("  user: {user}");
        }
    }
}

/// Print tags for elements that implement the standard `tags()` iterator.
fn print_tags<'a>(element: &impl HasTags<'a>) {
    let mut has_tags = false;
    for (k, v) in element.tags() {
        if !has_tags {
            println!("  tags:");
            has_tags = true;
        }
        println!("    {k} = {v}");
    }
}

fn print_tags_dense(dn: &crate::DenseNode<'_>) {
    let mut has_tags = false;
    for (k, v) in dn.tags() {
        if !has_tags {
            println!("  tags:");
            has_tags = true;
        }
        println!("    {k} = {v}");
    }
}

fn print_way_refs(w: &crate::Way<'_>) {
    let refs: Vec<i64> = w.refs().collect();
    if !refs.is_empty() {
        println!("  refs: ({} nodes)", refs.len());
        for id in &refs {
            println!("    {id}");
        }
    }
}

fn print_relation_members(r: &crate::Relation<'_>) {
    let members: Vec<_> = r.members().collect();
    if !members.is_empty() {
        println!("  members: ({})", members.len());
        for m in &members {
            let type_str = match m.id {
                crate::MemberId::Node(_) => "node",
                crate::MemberId::Way(_) => "way",
                crate::MemberId::Relation(_) => "relation",
                crate::MemberId::Unknown(_, _) => "unknown",
            };
            let role = m.role().unwrap_or("<invalid>");
            println!("    {type_str}/{} ({})", m.id.id(), role);
        }
    }
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
