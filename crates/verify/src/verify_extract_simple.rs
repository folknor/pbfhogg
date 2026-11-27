//! Verify extract's simple relation rule using osmium's complete_ways output.
//!
//! Complete_ways holds the candidates: every relation matched by the same
//! nodes and ways as simple, plus their parent relations. Once the simple
//! node and way sets agree, a candidate belongs in pbfhogg simple exactly
//! when one of its direct node or way members is in osmium simple. For a
//! pbfhogg-only relation, osmium's first node or way member must be absent.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::harness::Harness;
use crate::opl_compare::{self, Class, Merged, Policy, Stream, rank};
use crate::osc::OscKind;
use crate::pbf_frames::Origin;
use crate::reference_cache::Summary;
use crate::verify_element_diff::PerType;
use crate::verify_opl::{OplRelation, parse_relation};

/// The simple comparison has already judged order and established that
/// both simple outputs hold the same node and way IDs.
pub(super) struct SimpleRelations<'a> {
    pub label: &'a str,
    pub outdir: &'a Path,
    pub pbfhogg_simple: &'a Path,
    pub pbfhogg_simple_origin: Origin,
    pub osmium_simple: &'a Path,
    pub osmium_complete: &'a Path,
    pub complete_relations: u64,
    pub pbfhogg_only: &'a [i64],
}

pub(super) fn adjudicate(harness: &mut Harness, input: &SimpleRelations<'_>) -> Result<(), Error> {
    harness.msg(&format!(
        "=== verify {} relations against osmium complete_ways ===",
        input.label.trim_start_matches("pbfhogg ")
    ));
    let ours = (input.pbfhogg_simple, input.pbfhogg_simple_origin);
    let complete = (input.osmium_complete, Origin::Other);
    let policy = Policy::between(ours, complete)?;
    let sides = [
        (ours, policy),
        (complete, policy),
        // The old removeid PBF writer dropped the locations declaration.
        // Its comparison therefore printed deletions without way locations.
        ((input.osmium_simple, Origin::Other), Policy::NO_LOCATIONS),
    ];
    let listings = Listings::new(input.outdir);
    let result = opl_compare::with_decodings(harness, &sides, |h, held| {
        let paths = [held[0].as_path(), held[1].as_path(), held[2].as_path()];
        let (compared, summaries) = h.timed("opl merge", || {
            compare(paths, &listings, input.pbfhogg_only)
        })?;
        if compared.records != input.complete_relations {
            return Err(Error::Failed(format!(
                "{} holds {} relation records, but osmium fileinfo counted {} relations in {}",
                listings.relations.display(),
                compared.records,
                input.complete_relations,
                input.osmium_complete.display()
            )));
        }
        if let Some(members) = &compared.present {
            opl_compare::log_merge(h, &listings.present, members);
        }
        opl_compare::publish_extract_merge(h, paths, &summaries)?;
        opl_compare::log_merge(h, &listings.comparison, &compared.merged);
        Ok((compared, false))
    })?;
    result.report(harness, input)
}

struct Listings {
    comparison: PathBuf,
    relations: PathBuf,
    ids: PathBuf,
    present: PathBuf,
}

impl Listings {
    fn new(outdir: &Path) -> Self {
        Self {
            comparison: outdir.join("pbfhogg-simple-vs-osmium-complete.diff.txt"),
            relations: outdir.join("osmium-complete-relations.opl"),
            ids: outdir.join("complete-relation-members.ids"),
            present: outdir.join("osmium-simple-present-members.diff.txt"),
        }
    }
}

#[derive(Default)]
struct Compared {
    merged: Merged,
    present: Option<Merged>,
    only_simple: PerType,
    content: PerType,
    records: u64,
    pbfhogg_only_met: usize,
    judged: Judged,
}

impl Compared {
    fn report(&self, harness: &mut Harness, input: &SimpleRelations<'_>) -> Result<(), Error> {
        let label = input.label;
        if self.only_simple.total() > 0 {
            self.only_simple
                .print("FAIL ", "in simple but not in complete_ways", |line| {
                    harness.msg(line);
                });
            harness.fail(format!(
                "{label} holds {} elements osmium's complete_ways omits",
                self.only_simple.total()
            ));
        }
        if self.content.total() > 0 {
            self.content
                .print("FAIL ", "content differs from complete_ways", |line| {
                    harness.msg(line);
                });
            harness.fail(format!(
                "{label} element content differs from osmium's complete_ways on {} IDs",
                self.content.total()
            ));
        }
        if self.pbfhogg_only_met != input.pbfhogg_only.len() {
            return Err(Error::Failed(format!(
                "{} of the {} relations only pbfhogg's simple holds are not in osmium's \
                 complete_ways output, yet the simple vs complete_ways diff deleted none",
                input.pbfhogg_only.len() - self.pbfhogg_only_met,
                input.pbfhogg_only.len()
            )));
        }
        self.judged
            .report(harness, label, self.records, input.pbfhogg_only.len());
        Ok(())
    }
}

/// Walk the three decodings in object order. By the first candidate
/// relation, the third cursor has seen every node and way in osmium simple.
/// The index also keeps their OPL offsets for the deletion listing, so only
/// the records the listing needs are read back.
fn compare(
    [ours, complete, simple]: [&Path; 3],
    listings: &Listings,
    pbfhogg_only: &[i64],
) -> Result<(Compared, [Summary; 3]), Error> {
    let mut simple = SimpleMembers::open(simple)?;
    let mut relations = BufWriter::new(File::create(&listings.relations)?);
    let mut requested = [Vec::new(), Vec::new()];
    let mut result = Compared::default();
    let (merged, [ours, complete]) = opl_compare::merge_observed(
        Stream::open(ours)?,
        Stream::open(complete)?,
        &listings.comparison,
        true,
        |group| {
            let kind = [OscKind::Node, OscKind::Way, OscKind::Relation][group.kind];
            match group.class {
                Class::FirstOnly => result.only_simple.add(kind, group.id),
                Class::Differs => result.content.add(kind, group.id),
                Class::SecondOnly => {}
            }
        },
        |key, ours, complete| {
            simple.advance_through(key)?;
            if key.0 == OscKind::Relation.index()
                && let Some(line) = complete
            {
                let relation = parse_relation(line).map_err(|why| {
                    Error::Failed(format!(
                        "OPL {} record {}: {why}",
                        listings.relations.display(),
                        result.records + 1
                    ))
                })?;
                relations.write_all(line)?;
                relations.write_all(b"\n")?;
                result.records += 1;
                for &(kind, id) in &relation.members {
                    if let Some(ids) = requested.get_mut(kind.index()) {
                        ids.push(id);
                    }
                }
                let only = pbfhogg_only.binary_search(&relation.id).is_ok();
                result.pbfhogg_only_met += usize::from(only);
                result
                    .judged
                    .observe(&relation, &simple, ours.is_some(), only);
            }
            Ok(())
        },
    )?;
    relations.flush()?;
    simple.advance_through((OscKind::Relation.index(), i64::MAX))?;
    let summary = simple.stream.summary();
    result.present = simple.write_present(requested, listings, summary.counts)?;
    result.merged = merged;
    Ok((result, [ours, complete, summary]))
}

/// Sorted by kind and osmium ID order, with byte offsets into the simple
/// decoding. A relation member never selects another relation into simple.
struct SimpleMembers<'a> {
    path: &'a Path,
    stream: Stream,
    key: Option<(usize, i64)>,
    offset: u64,
    positions: [Vec<(i64, u64)>; 2],
}

impl<'a> SimpleMembers<'a> {
    fn open(path: &'a Path) -> Result<Self, Error> {
        let mut stream = Stream::open(path)?;
        let key = stream.next()?;
        Ok(Self {
            path,
            stream,
            key,
            offset: 0,
            positions: [Vec::new(), Vec::new()],
        })
    }

    fn advance_through(&mut self, through: (usize, i64)) -> Result<(), Error> {
        let through = rank(through);
        while let Some(key @ (kind, id)) = self.key
            && rank(key) <= through
        {
            if let Some(records) = self.positions.get_mut(kind) {
                records.push((id, self.offset));
            }
            self.offset += (self.stream.line().len() + 1) as u64;
            self.key = self.stream.next()?;
        }
        Ok(())
    }

    fn position(&self, kind: usize, id: i64) -> Option<u64> {
        let records = self.positions.get(kind)?;
        let index = records
            .binary_search_by_key(&rank((kind, id)), |&(id, _)| rank((kind, id)))
            .ok()?;
        Some(records[index].1)
    }

    fn has(&self, kind: OscKind, id: i64) -> bool {
        self.position(kind.index(), id).is_some()
    }

    /// Reproduce the old removeid diff's deletion lines. The sign gates
    /// already refused negative input IDs and negative output references.
    fn write_present(
        &self,
        requested: [Vec<i64>; 2],
        listings: &Listings,
        counts: [u64; 3],
    ) -> Result<Option<Merged>, Error> {
        if requested.iter().all(Vec::is_empty) {
            return Ok(None);
        }
        let mut ids_file = BufWriter::new(File::create(&listings.ids)?);
        let mut out = BufWriter::new(File::create(&listings.present)?);
        let mut opl =
            BufReader::with_capacity(crate::tunables::OPL_READ_BUFFER, File::open(self.path)?);
        let mut record = Vec::new();
        let mut cursor = 0;
        let mut merged = Merged {
            first: counts,
            second: counts,
            ..Merged::default()
        };
        for (kind, (prefix, mut ids)) in ["n", "w"].into_iter().zip(requested).enumerate() {
            ids.sort_unstable_by_key(|&id| rank((kind, id)));
            ids.dedup();
            for id in ids {
                writeln!(ids_file, "{prefix}{id}")?;
                if let Some(offset) = self.position(kind, id) {
                    let skip =
                        i64::try_from(offset - cursor).map_err(|e| Error::tool(e.to_string()))?;
                    opl.seek_relative(skip)?;
                    record.clear();
                    opl.read_until(b'\n', &mut record)?;
                    if !record.ends_with(b"\n") {
                        return Err(Error::Failed(format!(
                            "cannot read listed record at byte {offset} in {}",
                            self.path.display()
                        )));
                    }
                    out.write_all(b"-")?;
                    out.write_all(&record)?;
                    cursor = offset + record.len() as u64;
                    merged.first_only += 1;
                    merged.second[kind] -= 1;
                }
            }
        }
        ids_file.flush()?;
        out.flush()?;
        merged.same = merged.second.iter().sum();
        Ok(Some(merged))
    }
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

impl Judged {
    fn observe(
        &mut self,
        relation: &OplRelation,
        present: &SimpleMembers<'_>,
        in_simple: bool,
        only: bool,
    ) {
        let rule = belongs_in_simple(relation, present);
        self.expected += u64::from(rule);
        match (rule, in_simple) {
            (true, false) => self.missing.add(OscKind::Relation, relation.id),
            (false, true) => self.unmatched.add(OscKind::Relation, relation.id),
            _ => {}
        }
        if only && !first_member_outside(relation, present) {
            self.unexplained.add(OscKind::Relation, relation.id);
        }
    }

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

/// pbfhogg simple's rule: a direct node member in the node set or a direct
/// way member in the way set; relation members do not count.
fn belongs_in_simple(relation: &OplRelation, present: &SimpleMembers<'_>) -> bool {
    relation
        .members
        .iter()
        .any(|&(kind, id)| present.has(kind, id))
}

/// osmium simple's reason to omit a relation pbfhogg's simple writes: its
/// first node or way member is outside the sets (osmium decides on that
/// member alone). False for a relation with no node or way member.
fn first_member_outside(relation: &OplRelation, present: &SimpleMembers<'_>) -> bool {
    relation
        .members
        .iter()
        .find(|(kind, _)| *kind != OscKind::Relation)
        .is_some_and(|&(kind, id)| !present.has(kind, id))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn present_members_match_the_removeid_diff_and_keep_relations_typed() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let simple = dir.join("simple.opl");
        let complete = dir.join("complete.opl");
        let ours = dir.join("ours.opl");
        let without = dir.join("without.opl");
        let old_listing = dir.join("removeid.diff.txt");
        let listings = Listings::new(dir);
        let shared = "n0 T\nn2 T\nw2 T Nn2\nw3 T Nn2\n";
        std::fs::write(&simple, format!("{shared}r4 T Mn2@,w1@\n")).unwrap();
        std::fs::write(&ours, format!("{shared}r4 T Mn2@,w1@\nr5 T Mr4@,n9@,w3@\n")).unwrap();
        std::fs::write(
            &complete,
            "n0 T\nn2 T\nn9 T\nw2 T Nn2\nw3 T Nn2\n\
             r4 T Mn2@,w1@\nr5 T Mr4@,n9@,w3@\nr6 T Mn0@,n2@\nr7 T Mr4@\n",
        )
        .unwrap();
        // The requested n9 is absent. A request for w1 cannot match another
        // kind. removeid selects n0, n2 and w3, leaving w2 and relation 4.
        std::fs::write(&without, "w2 T Nn2\nr4 T Mn2@,w1@\n").unwrap();
        let (removed, _, _) = opl_compare::sequence_merge(&simple, &without, &old_listing).unwrap();
        let (result, _) = compare([&ours, &complete, &simple], &listings, &[5]).unwrap();

        assert_eq!(result.present, Some(removed));
        assert_eq!(
            std::fs::read(&listings.present).unwrap(),
            std::fs::read(old_listing).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&listings.comparison).unwrap(),
            "+n9 T\n+r6 T Mn0@,n2@\n+r7 T Mr4@\n"
        );
        assert_eq!(
            std::fs::read_to_string(&listings.ids).unwrap(),
            "n0\nn2\nn9\nw1\nw3\n"
        );
        assert_eq!(result.records, 4);
        assert_eq!(result.pbfhogg_only_met, 1);
        assert_eq!(result.judged.expected, 3);
        assert_eq!(result.judged.missing.kinds[2].ids(), vec![6]);
        assert_eq!(result.judged.unmatched.total(), 0);
        assert_eq!(result.judged.unexplained.total(), 0);
    }

    #[test]
    fn simple_relations_still_fail_when_only_pbfhogg_keeps_them_without_a_reason() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let simple = dir.join("simple.opl");
        let complete = dir.join("complete.opl");
        let ours = dir.join("ours.opl");
        let listings = Listings::new(dir);
        let with_relations = "n2 T\nr4 T Mr5@\nr5 T Mn2@\n";
        std::fs::write(&simple, "n2 T\n").unwrap();
        std::fs::write(&complete, with_relations).unwrap();
        std::fs::write(&ours, with_relations).unwrap();

        let (result, _) = compare([&ours, &complete, &simple], &listings, &[4, 5]).unwrap();

        assert!(result.merged.identical());
        assert_eq!(result.judged.missing.total(), 0);
        assert_eq!(result.judged.unmatched.kinds[2].ids(), vec![4]);
        assert_eq!(result.judged.unexplained.kinds[2].ids(), vec![4, 5]);
    }
}
