//! Shared owned element types used by derive_changes and diff commands.

use std::path::Path;

use crate::{BlobDecode, BlobReader, Element, MemberId};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// Owned element types — Vec fields are not converted to Box<[T]> because these
// are low-volume types (derive_changes/diff output), not hot-path allocations.
// ---------------------------------------------------------------------------

pub(crate) struct OwnedNode {
    pub(crate) id: i64,
    pub(crate) decimicro_lat: i32,
    pub(crate) decimicro_lon: i32,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) version: Option<i32>,
}

pub(crate) struct OwnedWay {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) refs: Vec<i64>,
    pub(crate) version: Option<i32>,
}

pub(crate) struct OwnedMember {
    pub(crate) id: MemberId,
    pub(crate) role: String,
}

pub(crate) struct OwnedRelation {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) members: Vec<OwnedMember>,
    pub(crate) version: Option<i32>,
}

// ---------------------------------------------------------------------------
// Reading PBF into owned vectors
// ---------------------------------------------------------------------------

pub(crate) struct ReadResult {
    pub(crate) nodes: Vec<OwnedNode>,
    pub(crate) ways: Vec<OwnedWay>,
    pub(crate) relations: Vec<OwnedRelation>,
}

pub(crate) fn read_elements(input: &Path, direct_io: bool) -> Result<ReadResult> {
    let reader = BlobReader::open(input, direct_io)?;
    let mut nodes: Vec<OwnedNode> = Vec::new();
    let mut ways: Vec<OwnedWay> = Vec::new();
    let mut relations: Vec<OwnedRelation> = Vec::new();

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(_) => {}
            BlobDecode::OsmData(block) => {
                for element in block.elements() {
                    match &element {
                        Element::DenseNode(dn) => {
                            nodes.push(OwnedNode {
                                id: dn.id(),
                                decimicro_lat: dn.decimicro_lat(),
                                decimicro_lon: dn.decimicro_lon(),
                                tags: dn
                                    .tags()
                                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                    .collect(),
                                version: dn.info().map(crate::dense::DenseNodeInfo::version),
                            });
                        }
                        Element::Node(n) => {
                            nodes.push(OwnedNode {
                                id: n.id(),
                                decimicro_lat: n.decimicro_lat(),
                                decimicro_lon: n.decimicro_lon(),
                                tags: n
                                    .tags()
                                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                    .collect(),
                                version: n.info().version(),
                            });
                        }
                        Element::Way(w) => {
                            ways.push(OwnedWay {
                                id: w.id(),
                                tags: w
                                    .tags()
                                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                    .collect(),
                                refs: w.refs().collect(),
                                version: w.info().version(),
                            });
                        }
                        Element::Relation(r) => {
                            relations.push(OwnedRelation {
                                id: r.id(),
                                tags: r
                                    .tags()
                                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                    .collect(),
                                members: r
                                    .members()
                                    .map(|m| OwnedMember {
                                        id: m.id,
                                        role: m.role().unwrap_or("").to_owned(),
                                    })
                                    .collect(),
                                version: r.info().version(),
                            });
                        }
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    Ok(ReadResult {
        nodes,
        ways,
        relations,
    })
}

// ---------------------------------------------------------------------------
// Element comparison
// ---------------------------------------------------------------------------

pub(crate) fn nodes_equal(a: &OwnedNode, b: &OwnedNode) -> bool {
    a.decimicro_lat == b.decimicro_lat
        && a.decimicro_lon == b.decimicro_lon
        && a.tags == b.tags
}

pub(crate) fn ways_equal(a: &OwnedWay, b: &OwnedWay) -> bool {
    a.refs == b.refs && a.tags == b.tags
}

pub(crate) fn members_equal(a: &[OwnedMember], b: &[OwnedMember]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(ma, mb)| ma.id == mb.id && ma.role == mb.role)
}

pub(crate) fn relations_equal(a: &OwnedRelation, b: &OwnedRelation) -> bool {
    a.tags == b.tags && members_equal(&a.members, &b.members)
}

// ---------------------------------------------------------------------------
// Coordinate conversion
// ---------------------------------------------------------------------------

pub(crate) fn from_decimicro(d: i32) -> f64 {
    f64::from(d) / 1e7
}

// ---------------------------------------------------------------------------
// Coordinate formatting
// ---------------------------------------------------------------------------

/// Format a coordinate, stripping unnecessary trailing zeros.
/// Writes directly into a provided buffer to avoid intermediate allocations.
pub(crate) fn format_coord(buf: &mut String, deg: f64) {
    use std::fmt::Write;
    buf.clear();
    // Use 7 decimal places (matches decimicrodegree precision)
    // write! to String is infallible (String::write_str never fails)
    write!(buf, "{deg:.7}").ok();
    let trimmed = buf.trim_end_matches('0').trim_end_matches('.');
    buf.truncate(trimmed.len());
}

// ---------------------------------------------------------------------------
// Clone helpers (owned types don't derive Clone to keep it explicit)
// ---------------------------------------------------------------------------

pub(crate) fn take_node(n: &OwnedNode) -> OwnedNode {
    OwnedNode {
        id: n.id,
        decimicro_lat: n.decimicro_lat,
        decimicro_lon: n.decimicro_lon,
        tags: n.tags.clone(),
        version: n.version,
    }
}

pub(crate) fn take_way(w: &OwnedWay) -> OwnedWay {
    OwnedWay {
        id: w.id,
        tags: w.tags.clone(),
        refs: w.refs.clone(),
        version: w.version,
    }
}

pub(crate) fn take_relation(r: &OwnedRelation) -> OwnedRelation {
    OwnedRelation {
        id: r.id,
        tags: r.tags.clone(),
        members: r
            .members
            .iter()
            .map(|m| OwnedMember {
                id: m.id,
                role: m.role.clone(),
            })
            .collect(),
        version: r.version,
    }
}
