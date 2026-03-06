//! XML-oriented owned element types for derive_changes, diff, merge_changes, and tags_filter_osc.
//!
//! Metadata fields are String-typed for direct XML attribute output.
//! See `elements_pbf` for the PBF-oriented variant with native types.

use crate::MemberId;

// ---------------------------------------------------------------------------
// Owned element types — Vec fields are not converted to Box<[T]> because these
// are low-volume types (derive_changes/diff output), not hot-path allocations.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct OwnedMetadata {
    pub(crate) version: i32,
    pub(crate) timestamp: String,
    pub(crate) changeset: String,
    pub(crate) uid: String,
    pub(crate) user: String,
    pub(crate) visible: String,
}

impl OwnedMetadata {
    pub(crate) fn version_only(version: i32) -> Self {
        Self {
            version,
            timestamp: String::new(),
            changeset: String::new(),
            uid: String::new(),
            user: String::new(),
            visible: String::new(),
        }
    }

    pub(crate) fn push_attrs(&self, elem: &mut quick_xml::events::BytesStart<'_>) {
        let v = self.version.to_string();
        elem.push_attribute(("version", v.as_str()));
        if !self.timestamp.is_empty() {
            elem.push_attribute(("timestamp", self.timestamp.as_str()));
        }
        if !self.changeset.is_empty() {
            elem.push_attribute(("changeset", self.changeset.as_str()));
        }
        if !self.uid.is_empty() {
            elem.push_attribute(("uid", self.uid.as_str()));
        }
        if !self.user.is_empty() {
            elem.push_attribute(("user", self.user.as_str()));
        }
        if !self.visible.is_empty() {
            elem.push_attribute(("visible", self.visible.as_str()));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedNode {
    pub(crate) id: i64,
    pub(crate) decimicro_lat: i32,
    pub(crate) decimicro_lon: i32,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedWay {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) refs: Vec<i64>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedMember {
    pub(crate) id: MemberId,
    pub(crate) role: String,
}

#[derive(Clone, Debug)]
pub(crate) struct OwnedRelation {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) members: Vec<OwnedMember>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

// ---------------------------------------------------------------------------
// Element comparison
// ---------------------------------------------------------------------------

pub(crate) fn nodes_equal(a: &OwnedNode, b: &OwnedNode) -> bool {
    a.decimicro_lat == b.decimicro_lat && a.decimicro_lon == b.decimicro_lon && a.tags == b.tags
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
