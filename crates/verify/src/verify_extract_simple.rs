//! Verify extract: simple's relation membership, judged against osmium's
//! complete_ways output.
//!
//! pbfhogg's simple writes a relation when any direct node member is in its
//! node set or any direct way member in its way set; osmium's simple decides
//! on the first node or way member only. So pbfhogg simple's relations are
//! not osmium simple's, and equal relation sets would not even prove pbfhogg
//! right (a regression to osmium's rule would pass). The expected set is
//! built instead from a reference that holds every candidate: osmium's
//! complete_ways output holds every relation matched by the same node and
//! way sets (the inside nodes and the ways with one of them, which simple's
//! node/way gate has already proven equal between the tools), plus their
//! parent relations. A relation of it belongs in simple exactly when one of
//! its direct node or way members is in simple's sets.
//!
//! The steps, each a reference run or a pbfhogg diff:
//! - the relations of osmium's complete_ways output as OPL
//!   (`osmium cat -t relation -f opl`), read by [`crate::verify_opl`], whose
//!   record count must equal the relation count inspect gave that output;
//! - which of their node and way members simple's sets hold: `osmium
//!   removeid` drops all of them from osmium's simple output, and a `pbfhogg
//!   diff --format osc` from that output to the result lists exactly the ones
//!   present, as deletes (anything else in it is an error);
//! - a `pbfhogg diff --format osc` from pbfhogg's simple output to osmium's
//!   complete_ways output: simple is a subset of complete_ways in every kind,
//!   so an element only pbfhogg holds is a FAIL, and a content difference is
//!   a FAIL for every element pbfhogg's simple writes, including the
//!   relations osmium's simple omits; the relations it creates are the
//!   complete_ways relations simple left out;
//! - per complete_ways relation, in or out of pbfhogg's simple output must
//!   match the rule, and every relation only pbfhogg's simple holds must have
//!   its first node or way member outside simple's sets, the documented
//!   reason osmium's simple omits it.

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::osc::{OscAction, OscKind};
use crate::reference::ToolRun;
use crate::verify_element_diff::{self as element_diff, PerType};
use crate::verify_opl::{self as opl, OplRelation};

/// What the adjudication reads: simple's outputs as compared, osmium's
/// complete_ways output with its inspected relation count, and the full,
/// sorted, distinct IDs of the relations only pbfhogg's simple holds.
pub(super) struct SimpleRelations<'a> {
    /// The arm's label in failures, `pbfhogg extract --simple` and the
    /// region it ran on.
    pub label: &'a str,
    pub outdir: &'a Path,
    pub pbfhogg_simple: &'a Path,
    pub osmium_simple: &'a Path,
    pub osmium_complete: &'a Path,
    pub complete_relations: u64,
    pub pbfhogg_only: &'a [i64],
}

/// Judge pbfhogg simple's relation membership; failures land in the harness,
/// a comparison that could not be made is an error.
pub(super) fn adjudicate(harness: &mut Harness, input: &SimpleRelations<'_>) -> Result<(), Error> {
    harness.msg(&format!(
        "=== verify {} relations against osmium complete_ways ===",
        input.label.trim_start_matches("pbfhogg ")
    ));
    let (relations, records, requested) = candidate_relations(harness, input)?;
    let present = present_members(harness, input.outdir, input.osmium_simple, &requested)?;
    let left_out = simple_vs_complete(harness, input)?;
    let judged = judge_relations(input, &relations, records, &left_out, &present)?;
    judged.report(harness, input.label, records, input.pbfhogg_only.len());
    Ok(())
}

/// osmium's complete_ways relations as OPL, their record count checked
/// against inspect's, and every node and way they reference.
fn candidate_relations(
    harness: &mut Harness,
    input: &SimpleRelations<'_>,
) -> Result<(PathBuf, u64, Typed), Error> {
    let outdir = input.outdir;
    let relations = outdir.join("osmium-complete-relations.opl");
    let captured = harness.reference(
        &ToolRun::osmium("extract-relations.1")
            .arg("cat")
            .input(input.osmium_complete)
            .args(&["-t", "relation", "-f", "opl", "-o"])
            .output(&relations)
            .arg("--overwrite"),
        &outdir.join("osmium-complete-relations.stdout"),
    )?;
    harness.require_success(&captured, "osmium cat -t relation")?;

    let mut requested = Typed::default();
    let records = opl::scan_relations(&relations, |r| {
        for &(kind, id) in &r.members {
            requested.push(kind, id);
        }
    })?;
    if records != input.complete_relations {
        return Err(Error::Failed(format!(
            "{} holds {records} relation records, but inspect counted {} relations in {}",
            relations.display(),
            input.complete_relations,
            input.osmium_complete.display()
        )));
    }
    requested.sort_distinct();
    // osmium removeid refuses negative IDs, so the presence of such a member
    // cannot be asked; that is a comparison that cannot be made, never an
    // absence. Production data holds none (pbfhogg rejects negative IDs on
    // most paths).
    if let Some((kind, id)) = requested.first_negative() {
        return Err(Error::Tool(format!(
            "osmium's complete_ways relations reference {kind} {id}; osmium removeid cannot look \
             up a negative ID, so simple's relations cannot be judged"
        )));
    }
    Ok((relations, records, requested))
}

/// Diff pbfhogg's simple output against osmium's complete_ways output: an
/// element only simple holds and a content difference are failures; returns
/// the complete_ways relations simple left out, sorted.
fn simple_vs_complete(
    harness: &mut Harness,
    input: &SimpleRelations<'_>,
) -> Result<Vec<i64>, Error> {
    let outdir = input.outdir;
    let mut only_simple = PerType::default();
    let mut content = PerType::default();
    let mut left_out = Vec::new();
    element_diff::scan_osc_diff(
        harness,
        input.pbfhogg_simple,
        input.osmium_complete,
        &outdir.join("pbfhogg-simple-vs-osmium-complete.osc"),
        |action, kind, id| match (action, kind) {
            (OscAction::Delete, _) => only_simple.add(kind, id),
            (OscAction::Modify, _) => content.add(kind, id),
            (OscAction::Create, OscKind::Relation) => left_out.push(id),
            (OscAction::Create, _) => {}
        },
    )?;
    left_out.sort_unstable();
    if left_out.windows(2).any(|w| w[0] == w[1]) {
        return Err(Error::Failed(
            "the simple vs complete_ways diff created one relation twice".into(),
        ));
    }
    let label = input.label;
    if only_simple.total() > 0 {
        only_simple.print("FAIL ", "in simple but not in complete_ways", |line| {
            harness.msg(line);
        });
        harness.fail(format!(
            "{label} holds {} elements osmium's complete_ways omits",
            only_simple.total()
        ));
    }
    if content.total() > 0 {
        content.print("FAIL ", "content differs from complete_ways", |line| {
            harness.msg(line);
        });
        harness.fail(format!(
            "{label} element content differs from osmium's complete_ways on {} IDs",
            content.total()
        ));
    }
    Ok(left_out)
}

/// Each complete_ways relation against simple's rule, and each relation only
/// pbfhogg's simple holds against osmium's first-member rule.
#[derive(Default)]
struct Judged {
    /// Relations with a direct node or way member in simple's sets.
    expected: u64,
    /// Relations the rule puts in simple that pbfhogg's simple omits.
    missing: PerType,
    /// Relations pbfhogg's simple holds without a member in its sets.
    unmatched: PerType,
    /// Relations only pbfhogg's simple holds whose first node or way member
    /// is in its sets, so osmium's rule does not explain the omission.
    unexplained: PerType,
}

/// Read the OPL relations again and judge each; an inventory that does not
/// account for every relation the diffs named is an error.
fn judge_relations(
    input: &SimpleRelations<'_>,
    relations: &Path,
    records: u64,
    left_out: &[i64],
    present: &Typed,
) -> Result<Judged, Error> {
    let mut judged = Judged::default();
    let mut left_out_met = 0;
    let mut pbfhogg_only_met = 0;
    let second = opl::scan_relations(relations, |r| {
        let in_simple = left_out.binary_search(&r.id).is_err();
        if !in_simple {
            left_out_met += 1;
        }
        let rule = belongs_in_simple(r, present);
        judged.expected += u64::from(rule);
        match (rule, in_simple) {
            (true, false) => judged.missing.add(OscKind::Relation, r.id),
            (false, true) => judged.unmatched.add(OscKind::Relation, r.id),
            _ => {}
        }
        if input.pbfhogg_only.binary_search(&r.id).is_ok() {
            pbfhogg_only_met += 1;
            if !first_member_outside(r, present) {
                judged.unexplained.add(OscKind::Relation, r.id);
            }
        }
    })?;
    if second != records {
        return Err(Error::Failed(format!(
            "{} read as {records} records, then as {second}",
            relations.display()
        )));
    }
    if left_out_met != left_out.len() {
        return Err(Error::Failed(format!(
            "the simple vs complete_ways diff created {} relations, {left_out_met} of them in {}",
            left_out.len(),
            relations.display()
        )));
    }
    if pbfhogg_only_met != input.pbfhogg_only.len() {
        return Err(Error::Failed(format!(
            "{} of the {} relations only pbfhogg's simple holds are not in osmium's \
             complete_ways output, yet the simple vs complete_ways diff deleted none",
            input.pbfhogg_only.len() - pbfhogg_only_met,
            input.pbfhogg_only.len()
        )));
    }
    Ok(judged)
}

impl Judged {
    /// Print the judgement and record its failures.
    fn report(&self, harness: &mut Harness, label: &str, records: u64, pbfhogg_only: usize) {
        let Self {
            expected,
            missing,
            unmatched,
            unexplained,
        } = self;
        harness.msg(&format!(
            "  complete_ways relations {records}: {expected} with a direct node or way member in \
             simple's node and way sets"
        ));
        let mut agree = true;
        if missing.total() > 0 {
            agree = false;
            missing.print("FAIL ", "relations simple must hold but omits", |line| {
                harness.msg(line);
            });
            harness.fail(format!(
                "{label} omits {} relations with a member in the extract",
                missing.total()
            ));
        }
        if unmatched.total() > 0 {
            agree = false;
            unmatched.print(
                "FAIL ",
                "relations without a member in the extract",
                |line| harness.msg(line),
            );
            harness.fail(format!(
                "{label} holds {} relations without a member in the extract",
                unmatched.total()
            ));
        }
        if unexplained.total() > 0 {
            agree = false;
            unexplained.print(
                "FAIL ",
                "pbfhogg-only relations whose first node or way member is in the extract",
                |line| harness.msg(line),
            );
            harness.fail(format!(
                "{label} holds {} relations osmium's simple omits for no documented reason",
                unexplained.total()
            ));
        }
        if agree {
            harness.msg(&format!(
                "  simple relations: pbfhogg holds exactly those {expected}; its {pbfhogg_only} \
             relations osmium's simple omits each have their first node or way member outside \
             the extract PASS"
            ));
        }
    }
}

/// Run osmium removeid of every `requested` member on osmium's simple output
/// and diff the result against it: the deletes are the members its node and
/// way sets hold. Any other entry, or a delete nobody asked for, is an error.
fn present_members(
    harness: &mut Harness,
    outdir: &Path,
    osmium_simple: &Path,
    requested: &Typed,
) -> Result<Typed, Error> {
    let mut present = Typed::default();
    if requested.is_empty() {
        return Ok(present);
    }
    let ids = outdir.join("complete-relation-members.ids");
    std::fs::write(&ids, requested.id_file().as_bytes())?;
    let without = outdir.join("osmium-simple-without-members.osm.pbf");
    let captured = harness.reference(
        &ToolRun::osmium("extract-removeid.1")
            .arg("removeid")
            .input(osmium_simple)
            .arg("-i")
            .input(&ids)
            .arg("-o")
            .output(&without)
            .arg("--overwrite"),
        &outdir.join("osmium-simple-without-members.stdout"),
    )?;
    harness.require_success(&captured, "osmium removeid")?;
    let mut stray: Option<String> = None;
    element_diff::scan_osc_diff(
        harness,
        osmium_simple,
        &without,
        &outdir.join("osmium-simple-present-members.osc"),
        |action, kind, id| {
            if action == OscAction::Delete && requested.has(kind, id) {
                present.push(kind, id);
            } else if stray.is_none() {
                stray = Some(format!("{action:?} {kind:?} {id}"));
            }
        },
    )?;
    if let Some(stray) = stray {
        return Err(Error::Failed(format!(
            "osmium removeid of the complete_ways relation members changed more than it was \
             asked to remove: {stray}"
        )));
    }
    present.sort_distinct();
    Ok(present)
}

/// Node and way IDs, kept apart by kind; relation references are not held.
#[derive(Default)]
struct Typed {
    nodes: Vec<i64>,
    ways: Vec<i64>,
}

impl Typed {
    fn push(&mut self, kind: OscKind, id: i64) {
        match kind {
            OscKind::Node => self.nodes.push(id),
            OscKind::Way => self.ways.push(id),
            OscKind::Relation => {}
        }
    }

    fn sort_distinct(&mut self) {
        for ids in [&mut self.nodes, &mut self.ways] {
            ids.sort_unstable();
            ids.dedup();
        }
    }

    /// Requires [`sort_distinct`](Self::sort_distinct) first.
    fn has(&self, kind: OscKind, id: i64) -> bool {
        match kind {
            OscKind::Node => self.nodes.binary_search(&id).is_ok(),
            OscKind::Way => self.ways.binary_search(&id).is_ok(),
            OscKind::Relation => false,
        }
    }

    /// The smallest negative ID held, by kind; requires
    /// [`sort_distinct`](Self::sort_distinct) first.
    fn first_negative(&self) -> Option<(&'static str, i64)> {
        [("node", &self.nodes), ("way", &self.ways)]
            .into_iter()
            .find_map(|(kind, ids)| ids.first().filter(|&&id| id < 0).map(|&id| (kind, id)))
    }

    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.ways.is_empty()
    }

    /// One typed ID per line (`n1`, `w2`), as osmium's `-i` reads them.
    fn id_file(&self) -> String {
        let mut text = String::new();
        for (prefix, ids) in [("n", &self.nodes), ("w", &self.ways)] {
            for id in ids {
                text.push_str(prefix);
                text.push_str(&id.to_string());
                text.push('\n');
            }
        }
        text
    }
}

/// pbfhogg simple's rule: a direct node member in the node set or a direct
/// way member in the way set; relation members do not count.
fn belongs_in_simple(relation: &OplRelation, present: &Typed) -> bool {
    relation
        .members
        .iter()
        .any(|&(kind, id)| present.has(kind, id))
}

/// osmium simple's reason to omit a relation pbfhogg's simple writes: its
/// first node or way member is outside the sets (osmium decides on that
/// member alone). False for a relation with no node or way member.
fn first_member_outside(relation: &OplRelation, present: &Typed) -> bool {
    relation
        .members
        .iter()
        .find(|(kind, _)| *kind != OscKind::Relation)
        .is_some_and(|&(kind, id)| !present.has(kind, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation(members: &[(OscKind, i64)]) -> OplRelation {
        OplRelation {
            id: 1,
            members: members.to_vec(),
        }
    }

    fn present() -> Typed {
        let mut present = Typed::default();
        present.push(OscKind::Node, 10);
        present.push(OscKind::Way, 20);
        // A relation reference is never held, even when pushed.
        present.push(OscKind::Relation, 30);
        present.sort_distinct();
        present
    }

    #[test]
    fn kinds_stay_apart() {
        let present = present();
        assert!(present.has(OscKind::Node, 10));
        assert!(!present.has(OscKind::Way, 10));
        assert!(!present.has(OscKind::Node, 20));
        assert!(!present.has(OscKind::Relation, 30));
        assert_eq!(present.id_file(), "n10\nw20\n");
        assert_eq!(present.first_negative(), None);
    }

    #[test]
    fn a_negative_member_is_named_not_queried() {
        let mut ids = Typed::default();
        ids.push(OscKind::Node, 5);
        ids.push(OscKind::Way, -3);
        ids.push(OscKind::Way, 0);
        ids.sort_distinct();
        assert_eq!(ids.first_negative(), Some(("way", -3)));
    }

    #[test]
    fn simple_takes_any_direct_node_or_way_member() {
        use OscKind::{Node, Relation, Way};
        let present = present();
        assert!(belongs_in_simple(
            &relation(&[(Way, 1), (Node, 10)]),
            &present
        ));
        assert!(belongs_in_simple(&relation(&[(Way, 20)]), &present));
        // A relation member never matches, nor does an ID of the other kind.
        assert!(!belongs_in_simple(
            &relation(&[(Relation, 30), (Way, 10)]),
            &present
        ));
        assert!(!belongs_in_simple(&relation(&[]), &present));
    }

    #[test]
    fn osmium_omits_on_the_first_node_or_way_member_only() {
        use OscKind::{Node, Relation, Way};
        let present = present();
        // First node-or-way member outside, a later one inside: the deviation.
        assert!(first_member_outside(
            &relation(&[(Node, 1), (Way, 20)]),
            &present
        ));
        // Relation members before it are skipped, as osmium skips them.
        assert!(first_member_outside(
            &relation(&[(Relation, 30), (Way, 2), (Node, 10)]),
            &present
        ));
        // First node-or-way member inside: osmium writes it too.
        assert!(!first_member_outside(
            &relation(&[(Relation, 5), (Node, 10), (Way, 3)]),
            &present
        ));
        // No node or way member: no reason osmium's rule gives.
        assert!(!first_member_outside(
            &relation(&[(Relation, 30)]),
            &present
        ));
    }
}
