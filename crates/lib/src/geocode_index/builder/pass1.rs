//! Pass 1: relation scan.
//!
//! Collects admin boundary metadata and the way IDs they reference.

use crate::{ElementKind, MemberId};

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
///
/// A way member's role decides whether the way is an outer ring, a hole or
/// auxiliary, so an eligible relation whose way member has a role string
/// index that resolves to no string (negative, or past the block's table) is
/// refused rather than read as an empty role: read as empty it would turn a
/// hole into another outer area. Roles of node and relation members, and of
/// relations that are not candidates, are never read and so not checked.
fn select_relation<'a>(
    rel: &crate::read::elements::Relation<'a>,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Option<PendingRelation>> {
    let Some(mut selected) = select_candidate(rel.id(), tags) else {
        return Ok(None);
    };
    for (position, m) in rel.members().enumerate() {
        if let MemberId::Way(wid) = m.id {
            match boundary_role(rel.member_role(position, &m)?) {
                BoundaryRole::Inner => selected.inner_way_ids.push(wid),
                BoundaryRole::Outer => selected.outer_way_ids.push(wid),
                BoundaryRole::Ignore => {}
            }
        }
    }
    Ok(Some(selected))
}

/// `rel` with no member ways yet when its tags make it a boundary
/// candidate.
fn select_candidate<'a>(
    id: i64,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> Option<PendingRelation> {
    let mut boundary: Option<&str> = None;
    let mut level_str: Option<&str> = None;
    let mut rel_name: Option<&str> = None;
    let mut cc: Option<&str> = None;
    let mut postal: Option<&str> = None;

    for (k, v) in tags {
        match BoundaryTag::classify(k.as_bytes()) {
            Some(BoundaryTag::Boundary) => boundary = Some(v),
            Some(BoundaryTag::Level) => level_str = Some(v),
            Some(BoundaryTag::Name) => rel_name = Some(v),
            Some(BoundaryTag::Country) => cc = Some(v),
            Some(BoundaryTag::Postcode) => postal = Some(v),
            None => {}
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
        if !super::super::format::is_administrative_level(l) {
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
    Some(PendingRelation {
        id,
        admin_level,
        name: name.into(),
        country_code,
        outer_way_ids: Vec::new(),
        inner_way_ids: Vec::new(),
    })
}

enum BoundaryTag {
    Boundary,
    Level,
    Name,
    Country,
    Postcode,
}

impl BoundaryTag {
    fn classify(key: &[u8]) -> Option<Self> {
        match key {
            b"boundary" => Some(Self::Boundary),
            b"admin_level" => Some(Self::Level),
            b"name" => Some(Self::Name),
            b"ISO3166-1:alpha2" => Some(Self::Country),
            b"postal_code" => Some(Self::Postcode),
            _ => None,
        }
    }
}

fn uses_tag(key: &[u8]) -> bool {
    BoundaryTag::classify(key).is_some()
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
    validation: super::InputValidation,
) -> Result<(Vec<RawAdminRelation>, crate::idset::IdSet)> {
    let mut admin_relations: Vec<RawAdminRelation> = Vec::new();
    crate::scan::classify::parallel_project_phase_ordered(
        file,
        relation_schedule,
        None,
        std::num::NonZeroUsize::new(crate::tunables::GEOCODE_MERGE_WINDOW_BLOBS),
        Vec::new,
        |buffer, matches| -> Result<Vec<PendingRelation>> {
            let mut pending = Vec::new();
            match validation {
                super::InputValidation::Scoped => {
                    buffer
                        .for_selection()?
                        .select_tag_values(ElementKind::Relation, uses_tag, matches)?
                        .visit_relations(|relation, tags| {
                            if let Some(selected) =
                                select_relation(&relation, tags.iter().copied())?
                            {
                                pending.push(selected);
                            }
                            Ok(())
                        })?;
                }
                #[cfg(test)]
                super::InputValidation::Full => {
                    let block = buffer.block()?;
                    for element in block.elements_skip_metadata() {
                        if let crate::Element::Relation(relation) = element
                            && let Some(selected) = select_relation(&relation, relation.tags())?
                        {
                            pending.push(selected);
                        }
                    }
                }
            }
            Ok(pending)
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

    // Build set of way IDs needed for admin boundary geometry, of either
    // sign.
    let mut needed_admin_ways = crate::idset::IdSet::new();
    for r in &admin_relations {
        for &wid in r.outer_way_ids.iter().chain(&r.inner_way_ids) {
            needed_admin_ways.insert(wid);
        }
    }

    Ok((admin_relations, needed_admin_ways))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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

    /// A way member role whose string index names no string is refused for
    /// a boundary candidate, naming the relation, the member's position in
    /// the full member list and the index; an unresolvable role on a node
    /// member, or on a relation that is not a candidate, is never read.
    #[test]
    fn an_unresolvable_way_role_of_a_boundary_is_refused() {
        use crate::read::wire_fixtures::{Group, block, field_bytes, packed, relation};

        // Strings: 0 "", 1 outer, 2 inner, 3 boundary, 4 administrative,
        // 5 admin_level, 6 8, 7 name, 8 X.
        let strings = [
            "",
            "outer",
            "inner",
            "boundary",
            "administrative",
            "admin_level",
            "8",
            "name",
            "X",
        ];
        let negative = u64::from_ne_bytes((-1i64).to_ne_bytes());
        let tagged = |mut r: Vec<u8>, tags: &[(u64, u64)]| {
            field_bytes(&mut r, 2, &packed(tags.iter().map(|t| t.0)));
            field_bytes(&mut r, 3, &packed(tags.iter().map(|t| t.1)));
            r
        };
        let boundary = [(3, 4), (5, 6), (7, 8)];
        let bytes = block(
            &strings,
            &[Group {
                relations: vec![
                    // Node member with a bad role, then valid ways.
                    tagged(
                        relation(1, &[(99, 5, 0), (1, 10, 1), (2, 11, 1)]),
                        &boundary,
                    ),
                    // Valid outer, then a way past the table at position 2.
                    tagged(
                        relation(2, &[(1, 10, 1), (0, 5, 0), (99, 11, 1)]),
                        &boundary,
                    ),
                    // A negative role index on a way.
                    tagged(relation(3, &[(negative, 10, 1)]), &boundary),
                    // Not a candidate: no name.
                    tagged(relation(4, &[(99, 10, 1)]), &boundary[..2]),
                ],
                ..Group::default()
            }],
        );
        let block = crate::PrimitiveBlock::from_vec(bytes).unwrap();
        let relations: Vec<_> = block
            .elements()
            .map(|element| match element {
                crate::Element::Relation(relation) => relation,
                _ => panic!("fixture contains only relations"),
            })
            .collect();

        let first = select_relation(&relations[0], relations[0].tags())
            .unwrap()
            .unwrap();
        assert_eq!(first.outer_way_ids, [10]);
        assert_eq!(first.inner_way_ids, [11]);

        let err = select_relation(&relations[1], relations[1].tags())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("relation 2 member 2: role string index 99"),
            "{err}"
        );
        let err = select_relation(&relations[2], relations[2].tags())
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("relation 3 member 0: role string index -1"),
            "{err}"
        );
        assert!(
            select_relation(&relations[3], relations[3].tags())
                .unwrap()
                .is_none()
        );
    }
}
