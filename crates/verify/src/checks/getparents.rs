//! Verify: getparents - pbfhogg getparents against osmium getparents, each
//! request with and without pbfhogg's `--add-self` (osmium's `-s`).
//!
//! Both tools select the same objects: a way that references a requested
//! node, a relation with a requested member of any kind, and with add-self
//! the requested objects themselves. osmium matches an ID by its absolute
//! value (`positive_ref`) and pbfhogg does not, so the comparison holds for
//! nonnegative IDs and references: an input osmium reads with a negative ID
//! is refused, and so is a negative way ref or relation member in any object
//! of any osmium output, which is where every match its folding adds would
//! show. The outputs are compared by ID ([`compare_outputs`]), so the input
//! osmium reads (the input, or its osmium-sorted copy on input without
//! `Sort.Type_then_ID`) must repeat no ID, which osmium's own reading of it
//! establishes before anything runs.
//!
//! The requests are per kind, since what a request holds decides which blob
//! kinds pbfhogg reads (nodes need way and relation blobs, ways and
//! relations relation blobs, add-self the requested kind's own too), so each
//! exercises its own combination: W, the lowest and highest way and the first way
//! member of the lowest relation holding one; R, the same for relations; N,
//! the lowest and highest node, the first node member of the lowest relation
//! holding one, and the first ref of a requested way, read from osmium's W
//! add-self output (so N holds a node with a way parent unless no requested
//! way has a ref); and M, all of them. Lowest and highest come from osmium's
//! reading of the input and exist; a member or ref need not exist, since a
//! parent is found by its reference. What the run exercised is read from
//! osmium's outputs as OPL, never pbfhogg's: a way parent of a requested
//! node, a relation parent of a requested node, way and relation, and for
//! each kind a requested object add-self adds that is no parent. Each one
//! absent is named in the check's PASS line.
//!
//! What a pass establishes is selection: element content and tags compared
//! by ID, not metadata, the header, or which blobs either tool skipped.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;
use crate::osc::OscKind;
use crate::reference::ToolRun;
use crate::verify_fileinfo::fileinfo;
use crate::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};
use crate::verify_opl::{self as opl, OplObject};

const KIND_LETTERS: [char; 3] = ['n', 'w', 'r'];
const KIND_NAMES: [&str; 3] = ["node", "way", "relation"];

/// One request: its name and the IDs it asks for as `(kind index, id)`.
struct Request {
    name: &'static str,
    ids: Vec<(usize, i64)>,
}

impl Request {
    fn args(&self) -> Vec<String> {
        self.ids
            .iter()
            .map(|&(kind, id)| format!("{}{id}", KIND_LETTERS[kind]))
            .collect()
    }

    fn contains(&self, kind: usize, id: i64) -> bool {
        self.ids.contains(&(kind, id))
    }
}

/// Add `id` of `kind` to `ids` unless it is there already or negative.
fn push(ids: &mut Vec<(usize, i64)>, kind: usize, id: i64) {
    if id >= 0 && !ids.contains(&(kind, id)) {
        ids.push((kind, id));
    }
}

/// The first nonnegative member of each kind, each from the lowest relation
/// holding one.
fn first_members(relations: &Path) -> Result<([Option<i64>; 3], u64), Error> {
    let mut first: [Option<i64>; 3] = [None; 3];
    let records = opl::scan_relations(relations, |r| {
        for &(kind, id) in &r.members {
            let slot = &mut first[kind.index()];
            if slot.is_none() && id >= 0 {
                *slot = Some(id);
            }
        }
    })?;
    Ok((first, records))
}

/// What one osmium output shows, read from it as OPL against its request.
#[derive(Debug, Default, PartialEq, Eq)]
struct Shown {
    /// A way referencing a requested node.
    way_of_node: bool,
    /// A relation with a requested member, per member kind.
    relation_of: [bool; 3],
    /// The requested objects present, as `(kind index, id)`.
    requested: Vec<(usize, i64)>,
    /// The first nonnegative ref of a requested way present.
    way_ref: Option<i64>,
}

/// Judge one output's objects against `request`; a negative ref or member
/// is an error.
fn judge(objects: &[OplObject], request: &Request) -> Result<Shown, String> {
    let mut shown = Shown::default();
    for object in objects {
        let kind = object.kind.index();
        if request.contains(kind, object.id) {
            shown.requested.push((kind, object.id));
            if kind == 1 && shown.way_ref.is_none() {
                shown.way_ref = object.nodes.iter().copied().find(|&n| n >= 0);
            }
        }
        for &node in &object.nodes {
            if node < 0 {
                return Err(format!(
                    "way {} references the negative node {node}",
                    object.id
                ));
            }
            shown.way_of_node |= request.contains(0, node);
        }
        for &(member_kind, id) in &object.members {
            if id < 0 {
                return Err(format!(
                    "relation {} has the negative {} member {id}",
                    object.id,
                    kind_name(member_kind)
                ));
            }
            shown.relation_of[member_kind.index()] |= request.contains(member_kind.index(), id);
        }
    }
    Ok(shown)
}

fn kind_name(kind: OscKind) -> &'static str {
    KIND_NAMES[kind.index()]
}

/// One request run by both tools in one mode, not yet compared.
struct Mode {
    label: String,
    osmium_label: String,
    ours: PathBuf,
    theirs: PathBuf,
    shown: Shown,
}

fn run_mode(
    harness: &mut Harness,
    outdir: &Path,
    pbf: &Path,
    order: &InputOrder,
    request: &Request,
    add_self: bool,
    direct_io: bool,
) -> Result<Mode, Error> {
    let args = request.args();
    let stem = format!(
        "getparents-{}{}",
        request.name.to_lowercase(),
        if add_self { "-add-self" } else { "" }
    );
    let label = format!(
        "getparents {}{}",
        request.name,
        if add_self { " --add-self" } else { "" }
    );
    let osmium_label = format!(
        "osmium getparents {}{}",
        request.name,
        if add_self { " -s" } else { "" }
    );
    harness.msg(&format!("--- {label}: {} ---", args.join(" ")));

    let ours = outdir.join(format!("pbfhogg-{stem}.osm.pbf"));
    let captured = harness.pbfhogg_discard(
        PbfArgv::new("getparents")
            .input(pbf)
            .output(&ours)
            .arg_if(add_self, "--add-self")
            .arg_if(direct_io, "--direct-io")
            .args_from(args.iter().map(String::as_str)),
    )?;
    harness.require_success(&captured, &format!("pbfhogg {label}"))?;

    let theirs = outdir.join(format!("osmium-{stem}.osm.pbf"));
    let mut run = ToolRun::osmium("getparents.1")
        .arg("getparents")
        .input(&order.reference_input);
    for id in &args {
        run = run.arg(id.as_str());
    }
    if add_self {
        run = run.arg("-s");
    }
    let run = run.arg("-o").output(&theirs).arg("--overwrite");
    let captured = harness.reference(&run, &outdir.join(format!("osmium-{stem}.stdout")))?;
    harness.require_success(&captured, &osmium_label)?;

    let listing = outdir.join(format!("osmium-{stem}.opl"));
    let captured = harness.reference(
        &ToolRun::osmium("getparents-opl.1")
            .arg("cat")
            .input(&theirs)
            .args(&["-f", "opl", "-o"])
            .output(&listing)
            .arg("--overwrite"),
        &outdir.join(format!("osmium-{stem}-opl.stdout")),
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
    let shown = judge(&objects, request).map_err(|why| {
        Error::Failed(format!(
            "{}: {why}; osmium matches a reference by its absolute value and pbfhogg does not, \
             so getparents cannot be compared on this input",
            listing.display()
        ))
    })?;
    Ok(Mode {
        label,
        osmium_label,
        ours,
        theirs,
        shown,
    })
}

/// The witnesses a request's plain and add-self runs leave missing.
fn missing(request: &Request, plain: &Shown, with_self: &Shown) -> Vec<String> {
    let mut missing = Vec::new();
    match request.name {
        "N" => {
            if !plain.way_of_node {
                missing.push("a way parent of a requested node".to_owned());
            }
            if !plain.relation_of[0] {
                missing.push("a relation parent of a requested node".to_owned());
            }
        }
        "W" if !plain.relation_of[1] => {
            missing.push("a relation parent of a requested way".to_owned());
        }
        "R" if !plain.relation_of[2] => {
            missing.push("a relation parent of a requested relation".to_owned());
        }
        _ => {}
    }
    let kind = match request.name {
        "N" => 0,
        "W" => 1,
        "R" => 2,
        _ => return missing,
    };
    let added = with_self
        .requested
        .iter()
        .any(|&(k, id)| k == kind && !plain.requested.contains(&(k, id)));
    if !added {
        missing.push(format!(
            "a requested {} that only --add-self selects",
            KIND_NAMES[kind]
        ));
    }
    missing
}

/// Run getparents cross-validation (module doc).
#[allow(clippy::too_many_lines)]
pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("getparents")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;

    harness.msg("--- getparents: reading the input osmium reads ---");
    let info = fileinfo(
        harness,
        &order.reference_input,
        &outdir.join("reference.fileinfo.json"),
    )?;
    if let Some((kind, id)) = info.negative_id() {
        return Err(Error::Failed(format!(
            "{} holds {} ID {id}; osmium getparents matches an ID by its absolute value and \
             pbfhogg does not, so getparents cannot be compared on it",
            order.reference_input.display(),
            KIND_NAMES[kind]
        )));
    }
    if !info.strictly_ordered() {
        return Err(Error::Failed(format!(
            "osmium reads {} out of strict order or with an ID repeated; getparents outputs are \
             compared by ID, which needs one object per kind and ID",
            order.reference_input.display()
        )));
    }

    let mut ids: [Vec<(usize, i64)>; 3] = Default::default();
    for (kind, ids) in ids.iter_mut().enumerate() {
        if let Some((min, max)) = info.range(kind) {
            push(ids, kind, min);
            push(ids, kind, max);
        }
    }
    if info.counts[2] > 0 {
        let relations = opl::input_relations(harness, &order.reference_input, &outdir)?;
        let (first, records) = first_members(&relations)?;
        if records != info.counts[2] {
            return Err(Error::Failed(format!(
                "{} holds {records} relation records, but osmium counts {} relations",
                relations.display(),
                info.counts[2]
            )));
        }
        for (kind, member) in first.into_iter().enumerate() {
            if let Some(id) = member {
                push(&mut ids[kind], kind, id);
            }
        }
    }

    let mut modes: Vec<Mode> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();
    let [nodes, ways, relations] = ids;
    let mut all: Vec<(usize, i64)> = Vec::new();

    // Both modes of one request; returns the first ref of a requested way
    // in osmium's add-self output.
    let run_request = |harness: &mut Harness,
                       request: &Request,
                       modes: &mut Vec<Mode>,
                       gaps: &mut Vec<String>|
     -> Result<Option<i64>, Error> {
        let plain = run_mode(harness, &outdir, pbf, &order, request, false, direct_io)?;
        let with_self = run_mode(harness, &outdir, pbf, &order, request, true, direct_io)?;
        gaps.extend(missing(request, &plain.shown, &with_self.shown));
        let way_ref = with_self.shown.way_ref;
        modes.push(plain);
        modes.push(with_self);
        Ok(way_ref)
    };

    let mut node_ids = nodes;
    let w = Request {
        name: "W",
        ids: ways,
    };
    if w.ids.is_empty() {
        gaps.push("a way request (no way ID to request)".to_owned());
    } else {
        match run_request(harness, &w, &mut modes, &mut gaps)? {
            Some(node) => push(&mut node_ids, 0, node),
            None => harness.msg("  no requested way has a ref to request"),
        }
    }
    let r = Request {
        name: "R",
        ids: relations,
    };
    if r.ids.is_empty() {
        gaps.push("a relation request (no relation ID to request)".to_owned());
    } else {
        run_request(harness, &r, &mut modes, &mut gaps)?;
    }
    let n = Request {
        name: "N",
        ids: node_ids,
    };
    if n.ids.is_empty() {
        gaps.push("a node request (no node ID to request)".to_owned());
    } else {
        run_request(harness, &n, &mut modes, &mut gaps)?;
    }
    for request in [&n, &w, &r] {
        for &(kind, id) in &request.ids {
            push(&mut all, kind, id);
        }
    }
    if all.is_empty() {
        return Err(Error::Failed(format!(
            "{} offers no ID to request; the comparison would be vacuous",
            pbf.display()
        )));
    }
    let m = Request {
        name: "M",
        ids: all,
    };
    run_request(harness, &m, &mut modes, &mut gaps)?;

    // Every osmium output was read, and a negative reference in any refused,
    // before any comparison records a verdict.
    for mode in &modes {
        compare_outputs(
            harness,
            &order,
            &mode.label,
            &mode.osmium_label,
            &mode.ours,
            &mode.theirs,
        )?;
    }

    let mut claim = Vec::new();
    if !order.sorted {
        claim.push(CLAIM_UNSORTED.to_owned());
    }
    if !gaps.is_empty() {
        claim.push(format!("not exercised on this input: {}", gaps.join(", ")));
        harness.msg(&format!(
            "  not exercised on this input: {}",
            gaps.join(", ")
        ));
    }
    Ok(if claim.is_empty() {
        None
    } else {
        Some(claim.join("; "))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use OscKind::{Node as N, Relation as R, Way as W};

    fn object(kind: OscKind, id: i64, refs: &[i64], members: &[(OscKind, i64)]) -> OplObject {
        OplObject {
            kind,
            id,
            tagged: false,
            nodes: refs.to_vec(),
            members: members.to_vec(),
        }
    }

    fn request(name: &'static str, ids: &[(usize, i64)]) -> Request {
        Request {
            name,
            ids: ids.to_vec(),
        }
    }

    #[test]
    fn judge_reads_each_witness_from_the_output() {
        let n = request("N", &[(0, 5), (0, 6)]);
        let shown = judge(
            &[
                object(W, 7, &[1, 5], &[]),
                object(R, 9, &[], &[(N, 6), (W, 3)]),
            ],
            &n,
        )
        .unwrap();
        assert!(shown.way_of_node);
        assert_eq!(shown.relation_of, [true, false, false]);
        assert!(shown.requested.is_empty());

        let w = request("W", &[(1, 7)]);
        let with_self = judge(&[object(W, 7, &[-0, 4], &[])], &w).unwrap();
        assert_eq!(with_self.requested, vec![(1, 7)]);
        assert_eq!(with_self.way_ref, Some(0));
        assert!(judge(&[object(W, 8, &[-4], &[])], &w).is_err());
        assert!(judge(&[object(R, 8, &[], &[(R, -1)])], &w).is_err());
    }

    #[test]
    fn missing_names_each_absent_witness() {
        let w = request("W", &[(1, 7)]);
        let plain = Shown {
            relation_of: [false, true, false],
            ..Shown::default()
        };
        let with_self = Shown {
            requested: vec![(1, 7)],
            ..Shown::default()
        };
        assert!(missing(&w, &plain, &with_self).is_empty());
        // add-self that adds nothing beyond the parents shows nothing.
        let parent_too = Shown {
            relation_of: [false, true, false],
            requested: vec![(1, 7)],
            ..Shown::default()
        };
        assert_eq!(
            missing(&w, &parent_too, &with_self),
            vec!["a requested way that only --add-self selects"]
        );
        let n = request("N", &[(0, 1)]);
        assert_eq!(
            missing(&n, &Shown::default(), &Shown::default()),
            vec![
                "a way parent of a requested node",
                "a relation parent of a requested node",
                "a requested node that only --add-self selects",
            ]
        );
        // The mixed request owes no witness of its own.
        let m = request("M", &[(0, 1)]);
        assert!(missing(&m, &Shown::default(), &Shown::default()).is_empty());
    }

    #[test]
    fn push_skips_negatives_and_repeats() {
        let mut ids = Vec::new();
        push(&mut ids, 0, 4);
        push(&mut ids, 0, 4);
        push(&mut ids, 0, -4);
        push(&mut ids, 1, 4);
        assert_eq!(ids, vec![(0, 4), (1, 4)]);
        assert_eq!(request("M", &ids).args(), vec!["n4", "w4"]);
    }
}
