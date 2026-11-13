//! Verify getid-removeid: `pbfhogg getid --add-referenced` against `osmium
//! getid -r`, plain and with pbfhogg's `--remove-tags` against osmium's
//! `-t`. pbfhogg follows references as osmium does (its DEVIATIONS.md,
//! "getid: missing objects are reported, not an exit status"): relation
//! members recursively through relations in relations, the node and way
//! members of every relation reached, then the nodes of every way requested
//! or pulled in; `--remove-tags` strips every pulled-in object that was not
//! itself requested.
//!
//! The request is its own, not the include endpoints: one relation R, the
//! highest way and the highest node the indexdata claims. R is chosen from
//! the input's relations (`osmium cat -t relation -f opl` of the reference
//! input, read twice by [`super::verify_opl`]) so that recursion shows in the
//! output: R holds a relation R2 other than itself that holds relations, and
//! R2 holds a relation R3 present in the input that is neither R nor a
//! direct member of R. The request holds no other relation, so R3 is in the
//! reference closure only through two hops and in no closure that stops at
//! R's direct members. The lowest such R whose members include both nodes
//! and ways is taken, else the lowest including one of them, else the lowest
//! such R; without one, the lowest relation by the same preference, and
//! recursion is not exercised.
//!
//! What a pass exercised is read from osmium's `-r` output (`osmium cat -f
//! opl`, read by [`super::verify_opl::scan_objects`]), never from
//! pbfhogg's: R3 present; a node, and a way, that is a member of a relation
//! in the output and not requested; a node in the output that is not
//! requested and no relation's node member, so only way-node expansion puts
//! it there; a pulled-in object with tags, which `-t` must strip; a
//! requested object with tags, which `-t` must keep. Each one absent is named
//! in the check's PASS line.
//!
//! osmium exits 1 when any requested or pulled-in object is not in the
//! input, the normal case on an extract, and also when an exception ends the
//! run. Its run is `-v`, and exit 1 completes it only when its verbose
//! output reports `Did not find N object(s).` followed by the final `Done.`,
//! which an exception skips ([`osmium_missing`]); the reference cache applies
//! the same condition before it stores or replays the run.
//!
//! The comparison holds for unique, non-negative object IDs. An input whose
//! indexdata claims a negative ID is refused: osmium matches an ID by its
//! absolute value and pbfhogg does not. A negative node or member reference
//! in any object of osmium's output is refused too: osmium folds it to its
//! absolute value, pbfhogg drops it, and every reference either tool follows
//! belongs to an object osmium wrote.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use super::pbf_frames::IndexSummary;
use super::verify_input_order::{InputOrder, compare_outputs};
use super::verify_opl::{self as opl, OplObject, OplRelation};
use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::osc::OscKind;
use crate::reference::ToolRun;

/// What [`run`] reads: the check's directory, the input and how osmium reads
/// it, the indexdata walk, and whether pbfhogg reads with O_DIRECT.
pub(crate) struct RefsInput<'a> {
    pub outdir: &'a Path,
    pub pbf: &'a Path,
    pub order: &'a InputOrder,
    pub summary: &'a IndexSummary,
    pub direct_io: bool,
}

/// Run both reference-following comparisons; failures are recorded on the harness.
/// Returns the claim parts naming what the input left unexercised (empty
/// when everything was).
pub(crate) fn run(harness: &mut Harness, input: &RefsInput<'_>) -> Result<Vec<String>, Error> {
    harness.msg("--- getid --add-referenced: pbfhogg vs osmium getid -r ---");
    refuse_negative_ids(input.summary, input.pbf)?;
    let choice = if input.summary.kinds[2].is_some() {
        choose_relation(harness, input)?
    } else {
        Choice::default()
    };
    match (choice.relation, choice.chain) {
        (Some(r), Some((r2, r3))) => {
            harness.msg(&format!(
                "  relation chain: r{r} holds r{r2}, which holds r{r3}"
            ));
        }
        (Some(r), None) => harness.msg(&format!(
            "  no relation chain two deep in the input; requesting r{r}"
        )),
        _ => harness.msg("  the input holds no relation"),
    }
    let request = request(input.summary, choice.relation);
    let args: Vec<String> = request
        .iter()
        .map(|&(kind, id)| format!("{}{id}", ["n", "w", "r"][kind]))
        .collect();
    harness.msg(&format!("  requesting: {}", args.join(" ")));

    // osmium's plain output is read, and a negative reference in it refused,
    // before either comparison records a verdict: such a reference makes the
    // tools differ by design, which is no pbfhogg failure.
    let plain = run_mode(harness, input, &args, false)?;
    let shown = exercised(harness, input.outdir, &plain.theirs, &request, choice.chain)?;
    plain.compare(harness, input.order)?;
    run_mode(harness, input, &args, true)?.compare(harness, input.order)?;
    let missing = shown.missing();
    if missing.is_empty() {
        harness.msg("  getid -r exercised every case it looks for");
        Ok(Vec::new())
    } else {
        harness.msg(&format!(
            "  getid -r on this input exercised no {}",
            missing.join(", ")
        ));
        Ok(vec![format!(
            "getid --add-referenced exercised no {} on this input",
            missing.join(", ")
        )])
    }
}

/// One mode, plain or `--remove-tags` (osmium's `-t`): both tools on `args`,
/// osmium's run judged by its completed verbose output.
fn run_mode(
    harness: &mut Harness,
    input: &RefsInput<'_>,
    args: &[String],
    remove_tags: bool,
) -> Result<Mode, Error> {
    let (label, stem, run_id, osmium_label) = if remove_tags {
        (
            "getid --add-referenced --remove-tags",
            "getid-r-remove-tags",
            "getid-rt.1",
            "osmium getid -r -t",
        )
    } else {
        (
            "getid --add-referenced",
            "getid-r",
            "getid-r.1",
            "osmium getid -r",
        )
    };
    let ours = input.outdir.join(format!("pbfhogg-{stem}.osm.pbf"));
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("getid")
            .arg("--add-referenced")
            .arg_if(remove_tags, "--remove-tags")
            .input(input.pbf)
            .output(&ours)
            .arg_if(input.direct_io, "--direct-io")
            .args_from(args.iter().map(String::as_str)),
    )?;
    harness.require_success(&captured, &format!("pbfhogg {label}"))?;

    let theirs = input.outdir.join(format!("osmium-{stem}.osm.pbf"));
    let mut osmium = ToolRun::osmium(run_id)
        .arg("getid")
        .input(&input.order.reference_input);
    for id in args {
        osmium = osmium.arg(id.as_str());
    }
    let osmium = osmium
        .args(if remove_tags {
            &["-r", "-t", "-v", "-o"]
        } else {
            &["-r", "-v", "-o"]
        })
        .output(&theirs)
        .arg("--overwrite")
        .accept_if(1, osmium_completed);
    let captured =
        harness.reference(&osmium, &input.outdir.join(format!("osmium-{stem}.stdout")))?;
    match captured.code() {
        Some(0) => harness.msg(&format!("  {osmium_label}: found every object")),
        Some(1) if osmium_completed(&captured.stderr) => harness.msg(&format!(
            "  {osmium_label}: did not find {} object(s) the request reached, its exit 1 by \
             design",
            osmium_missing(&captured.stderr).unwrap_or_default()
        )),
        Some(1) => {
            return Err(Error::Failed(format!(
                "{osmium_label} exited 1 without completing: its verbose output does not end \
                 in a missing-object count and Done.; stderr: {}",
                String::from_utf8_lossy(&captured.stderr).trim()
            )));
        }
        _ => {
            harness.require_success(&captured, osmium_label)?;
            return Err(Error::tool(format!(
                "{osmium_label} exited 0 after a non-zero status"
            )));
        }
    }
    Ok(Mode {
        label,
        osmium_label,
        ours,
        theirs,
    })
}

/// One mode's two outputs, not yet compared.
struct Mode {
    label: &'static str,
    osmium_label: &'static str,
    ours: PathBuf,
    theirs: PathBuf,
}

impl Mode {
    fn compare(&self, harness: &mut Harness, order: &InputOrder) -> Result<(), Error> {
        compare_outputs(
            harness,
            order,
            self.label,
            self.osmium_label,
            &self.ours,
            &self.theirs,
        )
    }
}

/// Refuse an input whose indexdata claims a negative ID of any kind.
fn refuse_negative_ids(summary: &IndexSummary, pbf: &Path) -> Result<(), Error> {
    for (kind, claim) in summary.kinds.iter().enumerate() {
        if let Some(claim) = claim
            && claim.min_id < 0
        {
            return Err(Error::Failed(format!(
                "{} claims {} ID {}; osmium getid -r matches an ID by its absolute value and \
                 pbfhogg does not, so getid --add-referenced cannot be compared on it",
                pbf.display(),
                ["node", "way", "relation"][kind],
                claim.min_id
            )));
        }
    }
    Ok(())
}

/// The request: the chosen relation, the highest claimed way and the
/// highest claimed node, as (kind index, ID), relations first.
fn request(summary: &IndexSummary, relation: Option<i64>) -> Vec<(usize, i64)> {
    let mut request = Vec::new();
    if let Some(r) = relation {
        request.push((2, r));
    }
    for kind in [1, 0] {
        if let Some(claim) = &summary.kinds[kind]
            && claim.max_id >= 0
        {
            request.push((kind, claim.max_id));
        }
    }
    request
}

/// The relation to request, and the chain `(R2, R3)` that makes its
/// recursion visible, when there is one.
#[derive(Debug, Default, PartialEq, Eq)]
struct Choice {
    relation: Option<i64>,
    chain: Option<(i64, i64)>,
}

/// The input's relations as OPL, read twice: the holders of relations,
/// then the presence of their members that hold none.
fn choose_relation(harness: &mut Harness, input: &RefsInput<'_>) -> Result<Choice, Error> {
    let relations = opl::input_relations(harness, &input.order.reference_input, input.outdir)?;
    let mut nesting = Nesting::default();
    let records = opl::scan_relations(&relations, |r| nesting.add(r))?;
    let claimed = input.summary.count(2);
    if records != claimed {
        return Err(Error::Failed(format!(
            "{} holds {records} relation records, but the input's indexdata claims {claimed} \
             relations",
            relations.display()
        )));
    }
    let pending = nesting.pending();
    if !pending.is_empty() {
        opl::scan_relations(&relations, |r| nesting.mark(&pending, r.id))?;
    }
    Ok(nesting.choose())
}

/// A relation holding relations: its distinct non-negative relation members,
/// sorted, and how many of the kinds node and way its members include.
struct Holder {
    children: Vec<i64>,
    kinds: usize,
}

/// What the choice of R needs from the input's relations.
#[derive(Default)]
struct Nesting {
    holders: BTreeMap<i64, Holder>,
    /// The lowest relation whose members include none, one or both of the
    /// kinds node and way, by that count.
    first_by_kinds: [Option<i64>; 3],
    /// Members of holders that hold no relation themselves and are records.
    present: HashSet<i64>,
}

impl Nesting {
    /// First pass: one relation record, in ascending ID order.
    fn add(&mut self, relation: &OplRelation) {
        let kinds = [OscKind::Node, OscKind::Way]
            .into_iter()
            .filter(|&kind| relation.members.iter().any(|&(k, _)| k == kind))
            .count();
        self.first_by_kinds[kinds].get_or_insert(relation.id);
        let mut children: Vec<i64> = relation
            .members
            .iter()
            .filter(|&&(kind, id)| kind == OscKind::Relation && id >= 0)
            .map(|&(_, id)| id)
            .collect();
        if children.is_empty() {
            return;
        }
        children.sort_unstable();
        children.dedup();
        self.holders.insert(relation.id, Holder { children, kinds });
    }

    /// Every member of a holder whose presence the first pass did not
    /// establish (a holder is a record, so present).
    fn pending(&self) -> HashSet<i64> {
        self.holders
            .values()
            .flat_map(|h| h.children.iter().copied())
            .filter(|id| !self.holders.contains_key(id))
            .collect()
    }

    /// Second pass: relation `id` is a record.
    fn mark(&mut self, pending: &HashSet<i64>, id: i64) {
        if pending.contains(&id) {
            self.present.insert(id);
        }
    }

    fn is_present(&self, id: i64) -> bool {
        self.holders.contains_key(&id) || self.present.contains(&id)
    }

    /// The chain `(R2, R3)` below `r`: R2 a member of `r` other than `r`
    /// that holds relations, R3 a member of R2 present in the input, neither
    /// `r` nor a member of `r`.
    fn chain(&self, r: i64, holder: &Holder) -> Option<(i64, i64)> {
        holder
            .children
            .iter()
            .filter(|&&r2| r2 != r)
            .filter_map(|&r2| self.holders.get(&r2).map(|h2| (r2, h2)))
            .find_map(|(r2, h2)| {
                h2.children
                    .iter()
                    .find(|&&r3| {
                        r3 != r
                            && holder.children.binary_search(&r3).is_err()
                            && self.is_present(r3)
                    })
                    .map(|&r3| (r2, r3))
            })
    }

    /// The lowest relation with a chain whose members include both nodes
    /// and ways, else one of them, else neither; without a chain, the lowest
    /// relation by the same preference.
    fn choose(&self) -> Choice {
        let mut by_kinds: [Option<Choice>; 3] = Default::default();
        for (&r, holder) in &self.holders {
            if by_kinds[holder.kinds].is_some() {
                continue;
            }
            let Some(chain) = self.chain(r, holder) else {
                continue;
            };
            by_kinds[holder.kinds] = Some(Choice {
                relation: Some(r),
                chain: Some(chain),
            });
            if holder.kinds == 2 {
                break;
            }
        }
        let [none, one, both] = by_kinds;
        both.or(one).or(none).unwrap_or(Choice {
            relation: self.first_by_kinds.iter().rev().find_map(|&r| r),
            chain: None,
        })
    }
}

/// osmium getid's verbose output ended a run that completed with objects
/// missing: a `Did not find N object(s).` line with N above zero, and after
/// it the final line `Done.`, both from its verbose stream. An exception
/// prints neither. `verify diff`'s candidate fetch admits its exit 1 by the
/// same rule.
pub(crate) fn osmium_completed(stderr: &[u8]) -> bool {
    osmium_missing(stderr).is_some()
}

/// The N of a completed run's `Did not find N object(s).` (see
/// [`osmium_completed`]).
pub(crate) fn osmium_missing(stderr: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(stderr).ok()?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let (last, before) = lines.split_last()?;
    if verbose_body(last)? != "Done." {
        return None;
    }
    before.iter().rev().find_map(|line| {
        let count = verbose_body(line)?
            .strip_prefix("Did not find ")?
            .strip_suffix(" object(s).")?;
        if !count.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        count.parse().ok().filter(|&n: &u64| n > 0)
    })
}

/// A line of libosmium's `VerboseOutput` without its elapsed-time prefix:
/// `[`, the minutes right-aligned in two columns, `:`, two-digit seconds,
/// `] `.
fn verbose_body(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?.trim_start_matches(' ');
    let (minutes, rest) = rest.split_once(':')?;
    if minutes.is_empty() || !minutes.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds = rest.get(..2)?;
    if !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.get(2..)?.strip_prefix("] ")
}

/// Which cases osmium's `-r` output shows the request exercised.
#[derive(Debug, Default, PartialEq, Eq)]
struct Exercised {
    recursion: bool,
    relation_node: bool,
    relation_way: bool,
    way_node: bool,
    pulled_tagged: bool,
    requested_tagged: bool,
}

impl Exercised {
    fn missing(&self) -> Vec<&'static str> {
        [
            (self.recursion, "recursion through a relation in a relation"),
            (self.relation_node, "pulled-in relation node member"),
            (self.relation_way, "pulled-in relation way member"),
            (self.way_node, "node pulled in by a way alone"),
            (self.pulled_tagged, "tagged pulled-in object"),
            (self.requested_tagged, "tagged requested object"),
        ]
        .into_iter()
        .filter_map(|(shown, name)| (!shown).then_some(name))
        .collect()
    }
}

/// osmium's `-r` output as OPL, read for what it shows exercised.
fn exercised(
    harness: &mut Harness,
    outdir: &Path,
    reference: &Path,
    request: &[(usize, i64)],
    chain: Option<(i64, i64)>,
) -> Result<Exercised, Error> {
    let listing = outdir.join("osmium-getid-r.opl");
    let captured = harness.reference(
        &ToolRun::osmium("getid-r-opl.1")
            .arg("cat")
            .input(reference)
            .args(&["-f", "opl", "-o"])
            .output(&listing)
            .arg("--overwrite"),
        &outdir.join("osmium-getid-r-opl.stdout"),
    )?;
    harness.require_success(&captured, "osmium cat -f opl")?;
    let mut objects = Vec::new();
    opl::scan_objects(&listing, |o| {
        objects.push(OplObject {
            kind: o.kind,
            id: o.id,
            tagged: o.tagged,
            nodes: o.nodes.clone(),
            members: o.members.clone(),
        });
    })?;
    let requested: BTreeSet<(usize, i64)> = request.iter().copied().collect();
    judge_exercised(&objects, &requested, chain.map(|(_, r3)| r3)).map_err(|why| {
        Error::Failed(format!(
            "{}: {why}; getid --add-referenced cannot be compared on it",
            listing.display()
        ))
    })
}

/// [`exercised`] over the output's objects; a negative reference in any of
/// them is an error.
fn judge_exercised(
    objects: &[OplObject],
    requested: &BTreeSet<(usize, i64)>,
    recursion_target: Option<i64>,
) -> Result<Exercised, String> {
    let present: HashSet<(usize, i64)> = objects.iter().map(|o| (o.kind.index(), o.id)).collect();
    let mut relation_nodes = HashSet::new();
    let mut shown = Exercised {
        recursion: recursion_target.is_some_and(|r3| present.contains(&(2, r3))),
        ..Exercised::default()
    };
    for object in objects {
        let key = (object.kind.index(), object.id);
        if object.tagged {
            if requested.contains(&key) {
                shown.requested_tagged = true;
            } else {
                shown.pulled_tagged = true;
            }
        }
        for &(kind, id) in &object.members {
            if id < 0 {
                return Err(format!(
                    "relation {} has the negative member {id}",
                    object.id
                ));
            }
            let member = (kind.index(), id);
            let pulled = present.contains(&member) && !requested.contains(&member);
            match kind {
                OscKind::Node => {
                    relation_nodes.insert(id);
                    shown.relation_node |= pulled;
                }
                OscKind::Way => shown.relation_way |= pulled,
                OscKind::Relation => {}
            }
        }
    }
    for object in objects {
        for &node in &object.nodes {
            if node < 0 {
                return Err(format!(
                    "way {} references the negative node {node}",
                    object.id
                ));
            }
            if present.contains(&(0, node))
                && !requested.contains(&(0, node))
                && !relation_nodes.contains(&node)
            {
                shown.way_node = true;
            }
        }
    }
    Ok(shown)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn relation(id: i64, members: &[(OscKind, i64)]) -> OplRelation {
        OplRelation {
            id,
            members: members.to_vec(),
        }
    }

    /// Choose over `relations` (ascending), both passes.
    fn choose(relations: &[OplRelation]) -> Choice {
        let mut nesting = Nesting::default();
        for r in relations {
            nesting.add(r);
        }
        let pending = nesting.pending();
        for r in relations {
            nesting.mark(&pending, r.id);
        }
        nesting.choose()
    }

    use OscKind::{Node as N, Relation as R, Way as W};

    #[test]
    fn a_chain_two_deep_is_chosen_and_one_holding_nodes_or_ways_preferred() {
        let relations = [
            relation(1, &[(R, 2)]),
            relation(2, &[(R, 3)]),
            relation(3, &[]),
            relation(4, &[(W, 9), (R, 5)]),
            relation(5, &[(R, 3)]),
            relation(6, &[(N, 8), (R, 5), (W, 9)]),
        ];
        assert_eq!(
            choose(&relations),
            Choice {
                relation: Some(6),
                chain: Some((5, 3))
            }
        );
        // One of the kinds beats neither.
        assert_eq!(
            choose(&relations[..5]),
            Choice {
                relation: Some(4),
                chain: Some((5, 3))
            }
        );
        // Without a node or way member, the lowest chain is taken.
        assert_eq!(
            choose(&relations[..3]),
            Choice {
                relation: Some(1),
                chain: Some((2, 3))
            }
        );
    }

    #[test]
    fn no_chain_without_a_present_grandchild_outside_the_direct_members() {
        // Self loop, a cycle back to R, an absent R3, and R3 a direct member
        // of R each leave nothing for recursion to show.
        for relations in [
            vec![relation(1, &[(R, 1), (N, 5)])],
            vec![relation(1, &[(R, 2), (N, 5)]), relation(2, &[(R, 1)])],
            vec![relation(1, &[(R, 2), (N, 5)]), relation(2, &[(R, 7)])],
            vec![
                relation(1, &[(R, 2), (R, 3), (N, 5)]),
                relation(2, &[(R, 3)]),
                relation(3, &[]),
            ],
        ] {
            let choice = choose(&relations);
            assert_eq!(
                choice.chain,
                None,
                "{:?}",
                relations.iter().map(|r| r.id).collect::<Vec<_>>()
            );
            assert_eq!(choice.relation, Some(1));
        }
        // No relation holds a node or way: the lowest relation.
        assert_eq!(
            choose(&[relation(3, &[]), relation(4, &[(R, 9)])]),
            Choice {
                relation: Some(3),
                chain: None
            }
        );
        assert_eq!(choose(&[]), Choice::default());
    }

    #[test]
    fn the_request_is_the_relation_then_the_highest_way_and_node() {
        use crate::pbf_frames::KindClaim;
        let claim = |min_id, max_id| {
            Some(KindClaim {
                min_id,
                max_id,
                count: 2,
            })
        };
        let summary = IndexSummary {
            kinds: [claim(1, 90), claim(2, 80), claim(3, 70)],
            required_features: Vec::new(),
        };
        assert_eq!(request(&summary, Some(7)), vec![(2, 7), (1, 80), (0, 90)]);
        let ways_only = IndexSummary {
            kinds: [None, claim(2, 80), None],
            required_features: Vec::new(),
        };
        assert_eq!(request(&ways_only, None), vec![(1, 80)]);
        assert!(refuse_negative_ids(&summary, Path::new("x.pbf")).is_ok());
        let negative = IndexSummary {
            kinds: [claim(-4, 90), None, None],
            required_features: Vec::new(),
        };
        let err = refuse_negative_ids(&negative, Path::new("x.pbf"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("node ID -4"), "{err}");
    }

    #[test]
    fn a_completed_run_is_told_from_an_exception() {
        let done = b"[ 0:00] Started osmium getid\n[ 0:01] Following references...\n\
                     [ 0:01] Done following references.\n[ 0:02] Did not find 12 object(s).\n\
                     [ 0:02] Peak memory used: 40 MBytes\n[ 0:02] Done.\n";
        assert_eq!(osmium_missing(done), Some(12));
        assert!(osmium_completed(done));
        let long = b"[123:59] Did not find 1 object(s).\n[123:59] Done.\n";
        assert_eq!(osmium_missing(long), Some(1));
        for bad in [
            // An exception: neither line.
            &b"[ 0:01] Following references...\nOut of memory.\n"[..],
            // The count without the final Done.
            b"[ 0:02] Did not find 3 object(s).\nerror\n",
            // Done following references is not Done.
            b"[ 0:02] Did not find 3 object(s).\n[ 0:02] Done following references.\n",
            // Done before the count.
            b"[ 0:02] Done.\n[ 0:02] Did not find 3 object(s).\n",
            // No verbose prefix.
            b"Did not find 3 object(s).\nDone.\n",
            // A zero count.
            b"[ 0:02] Did not find 0 object(s).\n[ 0:02] Done.\n",
            // Diagnostic text after Done.
            b"[ 0:02] Did not find 3 object(s).\n[ 0:02] Done.\nSegfault\n",
            b"",
        ] {
            assert_eq!(
                osmium_missing(bad),
                None,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    fn object(
        kind: OscKind,
        id: i64,
        tagged: bool,
        refs: &[i64],
        members: &[(OscKind, i64)],
    ) -> OplObject {
        OplObject {
            kind,
            id,
            tagged,
            nodes: refs.to_vec(),
            members: members.to_vec(),
        }
    }

    #[test]
    fn exercised_reads_each_case_from_the_reference_output() {
        let requested: BTreeSet<(usize, i64)> = [(2, 1), (1, 50), (0, 60)].into_iter().collect();
        let objects = [
            object(N, 5, false, &[], &[]),
            object(N, 6, true, &[], &[]),
            object(N, 60, true, &[], &[]),
            object(W, 7, false, &[5, 6], &[]),
            object(W, 50, false, &[60], &[]),
            object(R, 1, false, &[], &[(N, 5), (W, 7), (R, 2)]),
            object(R, 2, false, &[], &[(R, 3)]),
            object(R, 3, false, &[], &[]),
        ];
        assert_eq!(
            judge_exercised(&objects, &requested, Some(3)).unwrap(),
            Exercised {
                recursion: true,
                relation_node: true,
                relation_way: true,
                way_node: true,
                pulled_tagged: true,
                requested_tagged: true,
            }
        );
        // Node 6 is the only way-node witness; as a relation member it no
        // longer is, and a requested node never is.
        let objects_without = [
            object(N, 5, false, &[], &[]),
            object(N, 60, false, &[], &[]),
            object(W, 7, false, &[5], &[]),
            object(W, 50, false, &[60], &[]),
            object(R, 1, false, &[], &[(N, 5), (W, 7)]),
        ];
        let shown = judge_exercised(&objects_without, &requested, Some(3)).unwrap();
        assert_eq!(
            shown.missing(),
            vec![
                "recursion through a relation in a relation",
                "node pulled in by a way alone",
                "tagged pulled-in object",
                "tagged requested object",
            ]
        );
        let negative_member = [object(R, 1, false, &[], &[(N, -5)])];
        assert!(judge_exercised(&negative_member, &requested, None).is_err());
        let negative_node = [object(W, 50, false, &[-5], &[])];
        assert!(judge_exercised(&negative_node, &requested, None).is_err());
    }
}
