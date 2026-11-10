//! Pass 1: relation scan.
//!
//! Collects admin boundary metadata and the way IDs they reference.

use crate::{Element, MemberId};

use super::Result;
use super::strings::StringPool;

pub(super) struct RawAdminRelation {
    pub(super) id: i64,
    pub(super) admin_level: u8,
    pub(super) name_offset: u32,
    pub(super) country_code_offset: u32,
    pub(super) outer_way_ids: Vec<i64>,
    pub(super) inner_way_ids: Vec<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundaryRole {
    Outer,
    Inner,
    Ignore,
}

fn boundary_role(role: &str) -> BoundaryRole {
    // Relation roles that do not describe ring geometry, such as `label` or
    // `admin_centre`, are auxiliary members. Ignoring them must not discard a
    // valid boundary relation or turn one unfamiliar role into a build error.
    match role {
        "inner" => BoundaryRole::Inner,
        "outer" | "" => BoundaryRole::Outer,
        _ => BoundaryRole::Ignore,
    }
}

/// A selected boundary relation as a Pass 1 worker leaves it: its strings
/// owned, interned by the merge in file order.
struct PendingRelation {
    id: i64,
    admin_level: u8,
    name: Box<str>,
    /// Kept only at the country admin level, the one level that interns it.
    country_code: Option<Box<str>>,
    outer_way_ids: Vec<i64>,
    inner_way_ids: Vec<i64>,
}

/// The boundary relation `rel` contributes, or `None` when it is not one.
fn select_relation(rel: &crate::read::elements::Relation<'_>) -> Option<PendingRelation> {
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

    // This scan selects candidate boundary records. A missing or
    // unparseable admin level, an unsupported level, or a missing name makes
    // this relation ineligible; it does not make the source PBF unusable for
    // the other records that can still be indexed.
    let b = boundary?;
    let (is_admin, is_postal) = (b == "administrative", b == "postal_code");
    if !is_admin && !is_postal {
        return None;
    }

    let admin_level = if is_admin {
        let l = level_str?.parse::<u8>().ok()?;
        if !(crate::tunables::GEOCODE_ADMIN_LEVEL_MIN..=super::super::format::MAX_ADMIN_LEVEL)
            .contains(&l)
        {
            return None;
        }
        l
    } else {
        super::super::format::POSTAL_BOUNDARY_LEVEL
    };

    let name = if is_postal {
        postal.or(rel_name)
    } else {
        rel_name
    }?;
    let country_code = if admin_level == crate::tunables::GEOCODE_COUNTRY_ADMIN_LEVEL {
        cc.map(Into::into)
    } else {
        None
    };

    let mut outer_way_ids = Vec::new();
    let mut inner_way_ids = Vec::new();
    for m in rel.members() {
        if let MemberId::Way(wid) = m.id {
            let role = m.role().unwrap_or("");
            match boundary_role(role) {
                BoundaryRole::Inner => inner_way_ids.push(wid),
                BoundaryRole::Outer => outer_way_ids.push(wid),
                BoundaryRole::Ignore => {}
            }
        }
    }

    Some(PendingRelation {
        id: rel.id(),
        admin_level,
        name: name.into(),
        country_code,
        outer_way_ids,
        inner_way_ids,
    })
}

/// Scan relation blobs and build:
/// - the list of admin/postal boundary relations we care about,
/// - the set of way IDs those relations reference (outer + inner members).
///
/// Reads only the blobs of `relation_schedule` (the header walk's, see
/// `pass1_5::build_schedules`), decoded in parallel. The merge runs in
/// schedule (file) order and interns each relation's name, then its country
/// code, as it is reached, so pool offsets and the relation list come out in
/// the order a sequential scan gives. Entries carry their indexdata kind
/// claim, so a blob claiming relations that holds another kind is refused.
#[hotpath::measure]
pub(super) fn run_pass1(
    relation_schedule: &[crate::scan::classify::ScheduledBlob],
    file: &crate::read::input::InputFile,
    strings: &mut StringPool,
    host: &crate::host::Entered<'_>,
) -> Result<(Vec<RawAdminRelation>, crate::idset::IdSet)> {
    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    crate::scan::classify::parallel_classify_phase_ordered(
        file,
        relation_schedule,
        None,
        std::num::NonZeroUsize::new(crate::tunables::GEOCODE_MERGE_WINDOW_BLOBS),
        || (),
        |block, _: &mut ()| -> Result<Vec<PendingRelation>> {
            Ok(block
                .elements_skip_metadata()
                .filter_map(|element| match element {
                    Element::Relation(rel) => select_relation(&rel),
                    _ => None,
                })
                .collect())
        },
        |_position, pending: Vec<PendingRelation>| {
            for rel in pending {
                let name_offset = strings.intern(&rel.name);
                let country_code_offset =
                    rel.country_code.as_deref().map_or(0, |c| strings.intern(c));
                admin_relations.push(RawAdminRelation {
                    id: rel.id,
                    admin_level: rel.admin_level,
                    name_offset,
                    country_code_offset,
                    outer_way_ids: rel.outer_way_ids,
                    inner_way_ids: rel.inner_way_ids,
                });
            }
            Ok(())
        },
        host,
    )?;

    // Build set of way IDs needed for admin boundary geometry
    let mut needed_admin_ways = crate::idset::IdSet::new();
    for r in &admin_relations {
        for &wid in &r.outer_way_ids {
            needed_admin_ways.set(wid);
        }
        for &wid in &r.inner_way_ids {
            needed_admin_ways.set(wid);
        }
    }

    Ok((admin_relations, needed_admin_ways))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_libosmium_boundary_roles_contribute_geometry() {
        assert!(matches!(boundary_role(""), BoundaryRole::Outer));
        assert!(matches!(boundary_role("outer"), BoundaryRole::Outer));
        assert!(matches!(boundary_role("inner"), BoundaryRole::Inner));
        for role in ["subarea", "label", "admin_centre", "stop"] {
            assert!(matches!(boundary_role(role), BoundaryRole::Ignore));
        }
    }
}
