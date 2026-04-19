//! Pass 1: relation scan.
//!
//! Collects admin boundary metadata and the way IDs they reference.

use std::path::Path;

use crate::{BlobFilter, Element, ElementReader, MemberId};

use super::Result;
use super::strings::StringPool;

pub(super) struct RawAdminRelation {
    pub(super) admin_level: u8,
    pub(super) name_offset: u32,
    pub(super) country_code_offset: u32,
    pub(super) outer_way_ids: Vec<i64>,
    pub(super) inner_way_ids: Vec<i64>,
}

/// Scan relation blobs and build:
/// - the list of admin/postal boundary relations we care about,
/// - the set of way IDs those relations reference (outer + inner members).
#[hotpath::measure]
pub(super) fn run_pass1(
    input_path: &Path,
    direct_io: bool,
    strings: &mut StringPool,
) -> Result<(Vec<RawAdminRelation>, crate::idset::IdSet)> {
    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    {
        let reader = ElementReader::open(input_path, direct_io)?;
        reader.with_blob_filter(BlobFilter::only_relations())
            .for_each_block_pipelined(|block| {
            for element in block.elements_skip_metadata() {
                if let Element::Relation(rel) = element {
                    let mut boundary: Option<&str> = None;
                    let mut level_str: Option<&str> = None;
                    let mut rel_name: Option<&str> = None;
                    let mut cc: Option<&str> = None;
                    let mut postal: Option<&str> = None;

                    for (k, v) in rel.tags() {
                        match k {
                            "boundary" => boundary = Some(v),
                            "admin_level" => level_str = Some(v),
                            "name" => rel_name = Some(v),
                            "ISO3166-1:alpha2" => cc = Some(v),
                            "postal_code" => postal = Some(v),
                            _ => {}
                        }
                    }

                    let Some(b) = boundary else { continue };
                    let (is_admin, is_postal) = (b == "administrative", b == "postal_code");
                    if !is_admin && !is_postal { continue; }

                    let admin_level = if is_admin {
                        let Some(ls) = level_str else { continue };
                        let Ok(l) = ls.parse::<u8>() else { continue };
                        if !(2..=10).contains(&l) { continue; }
                        l
                    } else { 11 };

                    let name_str = if is_postal { postal.or(rel_name) } else { rel_name };
                    let Some(ns) = name_str else { continue };

                    let name_offset = strings.intern(ns);
                    let cc_offset = if admin_level == 2 { cc.map_or(0, |c| strings.intern(c)) } else { 0 };

                    let mut outer = Vec::new();
                    let mut inner = Vec::new();
                    for m in rel.members() {
                        if let MemberId::Way(wid) = m.id {
                            let role = m.role().unwrap_or("");
                            if role == "inner" { inner.push(wid); }
                            else { outer.push(wid); }
                        }
                    }

                    admin_relations.push(RawAdminRelation {
                        admin_level, name_offset, country_code_offset: cc_offset,
                        outer_way_ids: outer, inner_way_ids: inner,
                    });
                }
            }
            Ok(())
        })?;
    }

    // Build set of way IDs needed for admin boundary geometry
    let mut needed_admin_ways = crate::idset::IdSet::new();
    for r in &admin_relations {
        for &wid in &r.outer_way_ids { needed_admin_ways.set(wid); }
        for &wid in &r.inner_way_ids { needed_admin_ways.set(wid); }
    }

    Ok((admin_relations, needed_admin_ways))
}
