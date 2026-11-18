//! The negative-ID registry: every command path the binary parses, run on
//! the mixed-sign fixture graph and on its relabelled positive twin.
//!
//! A negative ID is an ID like any other. This file is where the commands are
//! held to that, path by path, so a command added without thought for
//! negative IDs fails here rather than passing unnoticed.
//!
//! ## Complete by construction
//!
//! `declared!` holds one entry per command path, keyed by the path's
//! words; it builds both the table and one `#[test]` per case in `entries`,
//! so a declared case always runs. `every_command_path_has_an_entry` compares the
//! declared paths with the clap tree production parses
//! (`invoker::subcommand_paths`) in both directions. An entry is either a
//! list of cases, one or more per ID-sensitive mode or flag, or a reasoned
//! `no_id_input` mark for a path that reads no element ID at all.
//!
//! ## The two arms
//!
//! Each case runs on the mixed graph (`common::mixed_sign`) and on its
//! twin, the same graph under an order-preserving bijection onto positive
//! IDs per kind (`Relabel`), every variant graph and side input (ID files,
//! OSC files, extract regions) built from the arm's own graph through that
//! one bijection. The expected result is written once, in the mixed graph's
//! IDs, and mapped for the twin; it is computed from the graph, never read
//! from another command's output, so two arms sharing a bug do not agree
//! their way to a pass. The twin arm is the positive control: it must pass
//! every check unconditionally, so a broken oracle or fixture fails the
//! test instead of hiding behind a gap. Each case runs in every framing its
//! command accepts: indexed, and unindexed (with `--force` where the
//! command refuses unindexed input without it).
//!
//! ## Known gaps
//!
//! A case whose mixed arm violates the rule keeps asserting the correct
//! answer and carries a [`Gap`] naming the check, the framing, the exact
//! wrong observation (an exit code and the diagnostic it prints, or the
//! exact element tokens missing and extra) and the reason. A gapped check
//! that fails with that observation passes and is printed; one that fails
//! differently fails, since the defect changed; one that passes fails and
//! asks for the marker to be removed, so a gap cannot outlive its fix.

// The oracles unwrap fixture and JSON plumbing; their verdicts are findings.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::common::mixed_sign::{
    Layout, MixedSignGraph, Relabel, changed_graph, dangling_graph, mixed_sign_graph, osc_between,
};
use crate::common::model::MemberKind;
use crate::common::{NormalizedElement, TestNode, TestRelation, TestWay, read_in_file_order};
use crate::invoker::{CliInvoker, subcommand_paths};
use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// One command path and what the registry holds for it.
struct Entry {
    path: &'static [&'static str],
    ids: Ids,
}

enum Ids {
    /// The path reads no element ID, with the reason.
    NoIdInput(&'static str),
    /// The cases, by name.
    Cases(Vec<NamedCase>),
}

type NamedCase = (&'static str, fn() -> Case);

/// The table [`declared`] holds, as [`Entry`] values.
macro_rules! registry_table {
    ($( [$($word:literal),+] => $kind:ident $args:tt ;)+) => {
        fn registry() -> Vec<Entry> {
            vec![$( registry_entry!([$($word),+] $kind $args) ),+]
        }
    };
}

/// One `#[test]` per case [`declared`] holds.
macro_rules! registry_cases {
    ($( [$($word:literal),+] => $kind:ident $args:tt ;)+) => {
        $( registry_tests!([$($word),+] $kind $args); )+
    };
}

macro_rules! registry_entry {
    ([$($word:literal),+] cases [$($case:ident),+ $(,)?]) => {
        Entry {
            path: &[$($word),+],
            ids: Ids::Cases(vec![$((stringify!($case), $case as fn() -> Case)),+]),
        }
    };
    ([$($word:literal),+] no_id_input ($reason:literal)) => {
        Entry { path: &[$($word),+], ids: Ids::NoIdInput($reason) }
    };
}

macro_rules! registry_tests {
    ($path:tt cases [$($case:ident),+ $(,)?]) => {
        $(
            #[test]
            fn $case() {
                super::run(&$path, stringify!($case), &super::$case());
            }
        )+
    };
    ($path:tt no_id_input ($reason:literal)) => {};
}

/// The registry: every command path and its cases, handed to `$then`.
macro_rules! declared {
    ($then:ident) => {
        $then! {
        ["cat"] => cases [cat_passthrough, cat_type_way, cat_clean, cat_dedupe];
        ["sort"] => cases [sort_reversed];
        ["repack"] => cases [repack_default];
        ["degrade"] => cases [degrade_unsort];
        ["renumber"] => cases [renumber_default, renumber_negative_start];
        ["tags-filter"] => cases [
            tags_filter_way_dependencies,
            tags_filter_relation_closure,
            tags_filter_omit_referenced,
            tags_filter_osc,
        ];
        ["export"] => cases [export_nodes, export_ways];
        ["diff"] => cases [diff_text, diff_osc];
        ["digest"] => cases [digest_full];
        ["getid"] => cases [
            getid_positional,
            getid_id_file,
            getid_id_osm_file,
            getid_add_referenced,
            getid_invert_node,
            getid_invert_way,
        ];
        ["getparents"] => cases [getparents_negative_query, getparents_negative_parents];
        ["extract"] => cases [extract_simple, extract_complete_ways, extract_smart, extract_config];
        ["add-locations-to-ways"] => cases [
            altw_sparse,
            altw_external,
            altw_auto_forced,
            altw_retention_and_present_refs,
            altw_true_misses,
            altw_true_miss_refused,
            altw_inject_prepass,
        ];
        ["time-filter"] => cases [time_filter_cutoff];
        ["inspect", "tags"] => cases [inspect_tags_counts];
        ["inspect"] => cases [
            inspect_report,
            inspect_extended,
            inspect_nodes,
            inspect_indexed,
            inspect_show,
        ];
        ["apply-changes"] => cases [
            apply_changes_default,
            apply_changes_create,
            apply_changes_locations_on_ways,
        ];
        ["merge-changes"] => cases [merge_changes_simplify];
        ["check"] => cases [
            check_default,
            check_ids,
            check_ids_full,
            check_ids_full_duplicate,
            check_refs_dangling,
            check_refs_relations_dangling,
            check_locations,
            check_locations_agreement,
            check_index,
        ];
        ["build-geocode-index"] => cases [build_geocode_index_admin];
        ["geocode-query"] => no_id_input(
            "takes a built index and coordinates; the IDs it was built from are \
build-geocode-index's, whose case queries the index it builds"
        );
        }
    };
}

declared!(registry_table);

/// One test per declared case.
mod entries {
    declared!(registry_cases);
}

#[test]
fn every_command_path_has_an_entry() {
    let tree: BTreeSet<Vec<String>> = subcommand_paths().into_iter().collect();
    let entries = registry();
    let mut problems = Vec::new();
    let mut declared = BTreeSet::new();
    let mut names = BTreeSet::new();
    for entry in &entries {
        let path: Vec<String> = entry.path.iter().map(|w| (*w).to_owned()).collect();
        if !declared.insert(path.clone()) {
            problems.push(format!("`{}` has two entries", path.join(" ")));
        }
        if !tree.contains(&path) {
            problems.push(format!(
                "`{}` has an entry but the binary has no such command path",
                path.join(" ")
            ));
        }
        match &entry.ids {
            Ids::NoIdInput(reason) => assert!(!reason.is_empty()),
            Ids::Cases(cases) => {
                if cases.is_empty() {
                    problems.push(format!("`{}` lists no case", path.join(" ")));
                }
                for (name, case) in cases {
                    if !names.insert(*name) {
                        problems.push(format!("case `{name}` is declared twice"));
                    }
                    let case = case();
                    if case.framings.is_empty() {
                        problems.push(format!("case `{name}` runs in no framing"));
                    }
                }
            }
        }
    }
    for path in &tree {
        if !declared.contains(path) {
            problems.push(format!(
                "`{}` has no negative-ID entry: add its cases, or a no_id_input mark \
                 saying why it reads no element ID",
                path.join(" ")
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

// ---------------------------------------------------------------------------
// Cases, arms, findings and gaps
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Indexed,
    Unindexed,
}

impl Framing {
    fn indexed(self) -> bool {
        self == Self::Indexed
    }
}

const BOTH: &[Framing] = &[Framing::Indexed, Framing::Unindexed];
const INDEXED: &[Framing] = &[Framing::Indexed];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    Mixed,
    Twin,
}

/// Which arms a case runs. `TwinOnly` is for a case whose subject is not
/// the input's sign (a negative argument on a positive input), judged by
/// an oracle of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arms {
    Both,
    TwinOnly,
}

/// One case: the arguments after the entry's path words, built in the arm's
/// scratch directory, and the judge of the run.
struct Case {
    framings: &'static [Framing],
    arms: Arms,
    argv: fn(&World, &Path, Framing) -> Vec<OsString>,
    judge: fn(&World, &Observed) -> Vec<Finding>,
    gaps: Vec<Gap>,
}

impl Case {
    fn both(
        framings: &'static [Framing],
        argv: fn(&World, &Path, Framing) -> Vec<OsString>,
        judge: fn(&World, &Observed) -> Vec<Finding>,
    ) -> Self {
        Self {
            framings,
            arms: Arms::Both,
            argv,
            judge,
            gaps: Vec::new(),
        }
    }

    fn gaps(self, gaps: Vec<Gap>) -> Self {
        Self { gaps, ..self }
    }
}

/// A known violation: the check it fails, where, and exactly how.
struct Gap {
    check: &'static str,
    arm: Arm,
    /// `None` for every framing the case runs.
    framing: Option<Framing>,
    sig: Sig,
    reason: &'static str,
}

const fn gap(check: &'static str, framing: Option<Framing>, sig: Sig, reason: &'static str) -> Gap {
    Gap {
        check,
        arm: Arm::Mixed,
        framing,
        sig,
        reason,
    }
}

/// The exact wrong observation a gap expects.
enum Sig {
    /// The run exited `code` and its output (stdout then stderr) holds `says`.
    Exit { code: i32, says: &'static str },
    /// The element tokens missing from and extra in the result, exactly.
    Tokens {
        missing: &'static [&'static str],
        extra: &'static [&'static str],
    },
}

#[derive(Debug)]
enum Discrepancy {
    Exit {
        code: Option<i32>,
        output: String,
    },
    Tokens {
        missing: BTreeSet<String>,
        extra: BTreeSet<String>,
    },
    Other(String),
}

impl Discrepancy {
    fn matches(&self, sig: &Sig) -> bool {
        match (self, sig) {
            (Self::Exit { code, output }, Sig::Exit { code: want, says }) => {
                *code == Some(*want) && output.contains(says)
            }
            (
                Self::Tokens { missing, extra },
                Sig::Tokens {
                    missing: m,
                    extra: e,
                },
            ) => {
                let set = |s: &[&str]| s.iter().map(|t| (*t).to_owned()).collect::<BTreeSet<_>>();
                *missing == set(m) && *extra == set(e)
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Discrepancy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exit { code, output } => write!(f, "exit {code:?}:\n{output}"),
            Self::Tokens { missing, extra } => {
                write!(f, "missing {missing:?}, extra {extra:?}")
            }
            Self::Other(text) => f.write_str(text),
        }
    }
}

struct Finding {
    check: &'static str,
    what: Discrepancy,
}

/// A finding of the check `check`.
fn finding(check: &'static str, what: Discrepancy) -> Finding {
    Finding { check, what }
}

/// One arm's run.
struct Observed {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    dir: PathBuf,
    framing: Framing,
}

impl Observed {
    fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

/// One arm's view of the graphs: the mixed IDs as they read in this arm.
struct World {
    relabel: Option<Relabel>,
}

impl World {
    fn new(arm: Arm) -> Self {
        let relabel = match arm {
            Arm::Mixed => None,
            Arm::Twin => Some(mixed_sign_graph().relabelled_twin().1),
        };
        Self { relabel }
    }

    fn n(&self, id: i64) -> i64 {
        self.relabel.as_ref().map_or(id, |r| r.node(id))
    }

    fn w(&self, id: i64) -> i64 {
        self.relabel.as_ref().map_or(id, |r| r.way(id))
    }

    fn r(&self, id: i64) -> i64 {
        self.relabel.as_ref().map_or(id, |r| r.relation(id))
    }

    /// `g` (in mixed IDs) as this arm holds it.
    fn graph(&self, g: MixedSignGraph) -> MixedSignGraph {
        match &self.relabel {
            None => g,
            Some(r) => g.relabelled(r),
        }
    }

    fn base(&self) -> MixedSignGraph {
        self.graph(mixed_sign_graph())
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Run one case: every framing it accepts, every arm, then the verdict.
fn run(path: &[&str], name: &str, case: &Case) {
    let arms: &[Arm] = match case.arms {
        Arms::Both => &[Arm::Mixed, Arm::Twin],
        Arms::TwinOnly => &[Arm::Twin],
    };
    let mut failures = Vec::new();
    // Every marker applies to a run the case makes, and no run has two
    // markers for one check.
    let mut marked = BTreeSet::new();
    for g in &case.gaps {
        let framings: Vec<Framing> = g.framing.map_or(case.framings.to_vec(), |f| vec![f]);
        if !arms.contains(&g.arm) || framings.iter().any(|f| !case.framings.contains(f)) {
            failures.push(format!(
                "[{name}] the gap on {} ({}) names a run the case does not make",
                g.check, g.reason
            ));
        }
        for f in framings {
            if !marked.insert((format!("{:?}", g.arm), format!("{f:?}"), g.check)) {
                failures.push(format!(
                    "[{name}] {:?} {f:?} {} has two gap markers",
                    g.arm, g.check
                ));
            }
        }
    }
    for &framing in case.framings {
        for &arm in arms {
            let world = World::new(arm);
            let scratch = pbfhogg_test_scratch::scratch();
            let dir = scratch.path().to_path_buf();
            let mut argv: Vec<OsString> = path.iter().map(OsString::from).collect();
            argv.extend((case.argv)(&world, &dir, framing));
            let out = CliInvoker::new(&scratch).args(&argv).run();
            let observed = Observed {
                code: out.status.code(),
                stdout: out.stdout_str(),
                stderr: out.stderr_str(),
                dir,
                framing,
            };
            assert!(
                !observed.stderr.contains("panicked at"),
                "[{name} {arm:?} {framing:?}] the command panicked:\n{}",
                observed.stderr
            );
            let findings = (case.judge)(&world, &observed);
            let gaps: Vec<&Gap> = case
                .gaps
                .iter()
                .filter(|g| g.arm == arm && g.framing.is_none_or(|f| f == framing))
                .collect();
            let here = format!("[{name} {arm:?} {framing:?}]");
            for f in &findings {
                match gaps.iter().find(|g| g.check == f.check) {
                    None => failures.push(format!("{here} {}: {}", f.check, f.what)),
                    Some(g) if f.what.matches(&g.sig) => {
                        let seen = f.what.to_string();
                        let seen: String = seen.chars().take(400).collect();
                        eprintln!("known gap {here} {}: {}\n  {seen}", f.check, g.reason);
                    }
                    Some(g) => failures.push(format!(
                        "{here} {} is marked as a known gap ({}) but fails differently: {}",
                        f.check, g.reason, f.what
                    )),
                }
            }
            for g in gaps {
                if !findings.iter().any(|f| f.check == g.check) {
                    failures.push(format!(
                        "{here} {} is marked as a known gap ({}) but passes: remove the marker",
                        g.check, g.reason
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

const IN: &str = "in.osm.pbf";
const OUT: &str = "out.osm.pbf";

/// Write `g` as `name` in `dir` under `layout`; the path as an argument.
fn write(dir: &Path, name: &str, g: &MixedSignGraph, layout: Layout) -> OsString {
    let path = dir.join(name);
    g.write_layout(&path, layout);
    path.into_os_string()
}

/// The arm's base graph as the input, in `framing`.
fn base_input(w: &World, dir: &Path, framing: Framing) -> OsString {
    write(dir, IN, &w.base(), Layout::plain(framing.indexed()))
}

fn out_arg(dir: &Path, name: &str) -> OsString {
    dir.join(name).into_os_string()
}

/// `--force` where a command refuses unindexed input without it.
fn force(framing: Framing) -> Vec<OsString> {
    if framing.indexed() {
        Vec::new()
    } else {
        os(&["--force"])
    }
}

/// `[input, -o, out]` plus `--force` when unindexed.
fn in_out_forced(w: &World, dir: &Path, framing: Framing) -> Vec<OsString> {
    let mut args = vec![base_input(w, dir, framing), "-o".into(), out_arg(dir, OUT)];
    args.extend(force(framing));
    args
}

fn in_out(w: &World, dir: &Path, framing: Framing) -> Vec<OsString> {
    vec![base_input(w, dir, framing), "-o".into(), out_arg(dir, OUT)]
}

// ---------------------------------------------------------------------------
// Tokens: what a result is compared as
// ---------------------------------------------------------------------------

fn tags_token<K: AsRef<str>, V: AsRef<str>>(tags: &[(K, V)]) -> String {
    tags.iter()
        .map(|(k, v)| format!("{}={}", k.as_ref(), v.as_ref()))
        .collect::<Vec<_>>()
        .join(";")
}

fn node_token(n: &TestNode) -> String {
    format!("n{} {},{} {}", n.id, n.lat, n.lon, tags_token(&n.tags))
}

fn way_token(g: &MixedSignGraph, w: &TestWay, locations: bool) -> String {
    let refs: Vec<String> = w.refs.iter().map(i64::to_string).collect();
    let locs = if locations {
        let coords: Vec<String> = w
            .refs
            .iter()
            .map(|r| {
                g.nodes
                    .iter()
                    .find(|n| n.id == *r)
                    .map_or("0,0".to_owned(), |n| format!("{},{}", n.lat, n.lon))
            })
            .collect();
        format!(" @{}", coords.join(";"))
    } else {
        String::new()
    };
    format!(
        "w{} [{}]{locs} {}",
        w.id,
        refs.join(","),
        tags_token(&w.tags)
    )
}

fn member_token(id: MemberId, role: &str) -> String {
    match id {
        MemberId::Node(id) => format!("n{id}:{role}"),
        MemberId::Way(id) => format!("w{id}:{role}"),
        MemberId::Relation(id) => format!("r{id}:{role}"),
        other => format!("{other:?}:{role}"),
    }
}

fn relation_token(r: &TestRelation) -> String {
    let members: Vec<String> = r
        .members
        .iter()
        .map(|m| member_token(m.id, m.role))
        .collect();
    format!("r{} [{}] {}", r.id, members.join(","), tags_token(&r.tags))
}

/// A selection of a graph's elements, by ID in the arm's space; `None`
/// keeps every element of the kind.
struct Pick<'a> {
    nodes: Option<&'a [i64]>,
    ways: Option<&'a [i64]>,
    relations: Option<&'a [i64]>,
    locations: bool,
}

const ALL: Pick<'static> = Pick {
    nodes: None,
    ways: None,
    relations: None,
    locations: false,
};

/// The tokens of the picked elements of `g`, in `g`'s (canonical) order.
fn expect(g: &MixedSignGraph, pick: &Pick) -> Vec<String> {
    let keep = |sel: Option<&[i64]>, id: i64| sel.is_none_or(|s| s.contains(&id));
    let mut out = Vec::new();
    out.extend(
        g.nodes
            .iter()
            .filter(|n| keep(pick.nodes, n.id))
            .map(node_token),
    );
    out.extend(
        g.ways
            .iter()
            .filter(|w| keep(pick.ways, w.id))
            .map(|w| way_token(g, w, pick.locations)),
    );
    out.extend(
        g.relations
            .iter()
            .filter(|r| keep(pick.relations, r.id))
            .map(relation_token),
    );
    out
}

/// The tokens of a PBF's elements in file order, inline locations included.
fn observe(path: &Path) -> Vec<String> {
    read_in_file_order(path)
        .into_iter()
        .map(|e| match e {
            NormalizedElement::Node(n) => format!(
                "n{} {},{} {}",
                n.id,
                n.decimicro_lat(),
                n.decimicro_lon(),
                tags_token(&n.tags)
            ),
            NormalizedElement::Way(w) => {
                let refs: Vec<String> = w.refs.iter().map(i64::to_string).collect();
                let locs = w.locations.as_ref().map_or(String::new(), |l| {
                    let coords: Vec<String> = l
                        .iter()
                        .map(|(lat, lon)| format!("{},{}", lat / 100, lon / 100))
                        .collect();
                    format!(" @{}", coords.join(";"))
                });
                format!(
                    "w{} [{}]{locs} {}",
                    w.id,
                    refs.join(","),
                    tags_token(&w.tags)
                )
            }
            NormalizedElement::Relation(r) => {
                let members: Vec<String> = r
                    .members
                    .iter()
                    .map(|m| {
                        let kind = match m.member_type {
                            MemberKind::Node => "n".to_owned(),
                            MemberKind::Way => "w".to_owned(),
                            MemberKind::Relation => "r".to_owned(),
                            MemberKind::Unknown(raw) => format!("u{raw}/"),
                        };
                        format!("{kind}{}:{}", m.ref_id, m.role)
                    })
                    .collect();
                format!("r{} [{}] {}", r.id, members.join(","), tags_token(&r.tags))
            }
        })
        .collect()
}

/// Compare token lists: as multisets (`tokens`), then, when `ordered`, as
/// sequences (`order`).
fn compare(
    check: &'static str,
    expected: &[String],
    observed: &[String],
    ordered: bool,
) -> Vec<Finding> {
    let count = |v: &[String]| {
        let mut m: BTreeMap<String, usize> = BTreeMap::new();
        for t in v {
            *m.entry(t.clone()).or_default() += 1;
        }
        m
    };
    let (want, got) = (count(expected), count(observed));
    // A token short or over by more than one copy carries its count, so a
    // duplicated wrong element is not the same observation as one.
    let delta = |t: &str, by: usize| {
        if by == 1 {
            t.to_owned()
        } else {
            format!("{t} x{by}")
        }
    };
    let missing: BTreeSet<String> = want
        .iter()
        .filter_map(|(t, n)| {
            let have = got.get(t).copied().unwrap_or(0);
            (have < *n).then(|| delta(t, n - have))
        })
        .collect();
    let extra: BTreeSet<String> = got
        .iter()
        .filter_map(|(t, n)| {
            let want = want.get(t).copied().unwrap_or(0);
            (want < *n).then(|| delta(t, n - want))
        })
        .collect();
    let mut findings = Vec::new();
    if !missing.is_empty() || !extra.is_empty() {
        findings.push(finding(check, Discrepancy::Tokens { missing, extra }));
    }
    // The order of what both hold is judged whatever else differs, so a
    // known gap in the contents cannot hide a reordering.
    if ordered {
        let common = |of: &[String], other: &BTreeMap<String, usize>| -> Vec<String> {
            of.iter()
                .filter(|t| other.contains_key(*t))
                .cloned()
                .collect()
        };
        let (a, b) = (common(expected, &got), common(observed, &want));
        if a != b {
            findings.push(finding(
                "order",
                Discrepancy::Other(format!("expected {a:?}\nobserved {b:?}")),
            ));
        }
    }
    findings
}

/// The exit check: `code` wanted.
fn exit(o: &Observed, code: i32) -> Option<Finding> {
    (o.code != Some(code)).then(|| {
        finding(
            "exit",
            Discrepancy::Exit {
                code: o.code,
                output: o.output(),
            },
        )
    })
}

/// A PBF-writing run: exit 0, and `name`'s elements equal to `expected`
/// (in that order when `ordered`). Without the output the element check
/// has no evidence and is not judged; the exit finding stands for it.
fn pbf_result(o: &Observed, name: &str, expected: &[String], ordered: bool) -> Vec<Finding> {
    let (mut findings, path) = artifact(o, name, 0);
    if let Some(path) = path {
        findings.extend(compare("elements", expected, &observe(&path), ordered));
    }
    findings
}

/// The exit check for a run that publishes `name` when it exits `code`,
/// and the artifact when it exists. A run exiting `code` without it is a
/// finding of its own; a run that exits otherwise and publishes it anyway
/// has its artifact judged like any other.
fn artifact(o: &Observed, name: &str, code: i32) -> (Vec<Finding>, Option<PathBuf>) {
    let mut findings: Vec<Finding> = exit(o, code).into_iter().collect();
    let path = o.file(name);
    if path.exists() {
        if o.code != Some(code) {
            findings.push(finding(
                "published",
                Discrepancy::Other(format!("exited {:?} and still published {name}", o.code)),
            ));
        }
        return (findings, Some(path));
    }
    if o.code == Some(code) {
        findings.push(finding(
            "artifact",
            Discrepancy::Other(format!("exited {code} without publishing {name}")),
        ));
    }
    (findings, None)
}

// ---------------------------------------------------------------------------
// cat, sort, repack, degrade, time-filter: every element survives
// ---------------------------------------------------------------------------

fn judge_all(w: &World, o: &Observed) -> Vec<Finding> {
    pbf_result(o, OUT, &expect(&w.base(), &ALL), true)
}

fn cat_passthrough() -> Case {
    Case::both(BOTH, in_out, judge_all)
}

fn cat_type_way() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.extend(os(&["-t", "way"]));
            a
        },
        |w, o| {
            let pick = Pick {
                nodes: Some(&[]),
                relations: Some(&[]),
                ..ALL
            };
            pbf_result(o, OUT, &expect(&w.base(), &pick), true)
        },
    )
}

/// The clean pass re-encodes every element.
fn cat_clean() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out(w, dir, f);
            a.extend(os(&["--clean", "version"]));
            a
        },
        judge_all,
    )
}

fn cat_dedupe() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let input = base_input(w, dir, f);
            let mut a = vec![
                "--dedupe".into(),
                input.clone(),
                input,
                "-o".into(),
                out_arg(dir, OUT),
            ];
            a.extend(force(f));
            a
        },
        judge_all,
    )
}

fn sort_reversed() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let layout = Layout {
                indexdata: f.indexed(),
                sorted: false,
                locations: false,
            };
            let mut a = vec![
                write(dir, IN, &w.base(), layout),
                "-o".into(),
                out_arg(dir, OUT),
            ];
            a.extend(force(f));
            a
        },
        judge_all,
    )
}

fn repack_default() -> Case {
    Case::both(BOTH, in_out_forced, judge_all)
}

fn degrade_unsort() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.extend(os(&["--unsort", "--elements-per-blob", "2"]));
            a
        },
        // The order is the perturbation's; the elements are the input's.
        |w, o| pbf_result(o, OUT, &expect(&w.base(), &ALL), false),
    )
}

/// Every element stamped before the cutoff but `n-7`, stamped after it: the
/// snapshot drops `n-7` and keeps its magnitude twin `n7`.
fn time_filter_cutoff() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut g = w.base();
            let stamp = |at: i64| {
                Some(crate::common::TestMeta {
                    timestamp: at,
                    ..Default::default()
                })
            };
            for n in &mut g.nodes {
                n.meta = stamp(if n.id == w.n(-7) {
                    2_000_000_000
                } else {
                    1_000_000_000
                });
            }
            for x in &mut g.ways {
                x.meta = stamp(1_000_000_000);
            }
            for r in &mut g.relations {
                r.meta = stamp(1_000_000_000);
            }
            vec![
                write(dir, IN, &g, Layout::plain(f.indexed())),
                "-o".into(),
                out_arg(dir, OUT),
                "1500000000".into(),
            ]
        },
        |w, o| {
            let g = w.base();
            let nodes: Vec<i64> = g
                .nodes
                .iter()
                .map(|n| n.id)
                .filter(|&id| id != w.n(-7))
                .collect();
            let pick = Pick {
                nodes: Some(&nodes),
                ..ALL
            };
            pbf_result(o, OUT, &expect(&g, &pick), true)
        },
    )
}

// ---------------------------------------------------------------------------
// renumber
// ---------------------------------------------------------------------------

/// The default renumbering assigns each kind's IDs by canonical rank from 1,
/// which is exactly the twin's bijection: both arms renumber to the twin.
fn renumber_default() -> Case {
    Case::both(INDEXED, in_out, |_, o| {
        let twin = mixed_sign_graph().relabelled_twin().0;
        pbf_result(o, OUT, &expect(&twin, &ALL), true)
    })
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            says: "renumber requires non-negative input ids. Input contains node id -1.",
        },
        "renumber refuses negative source IDs",
    )])
}

/// A negative start counts down from it, as osmium's does: on the positive
/// twin, `--start-id -1` gives every kind `-1, -2, ...` by canonical rank.
fn renumber_negative_start() -> Case {
    Case {
        arms: Arms::TwinOnly,
        gaps: vec![Gap {
            check: "exit",
            arm: Arm::Twin,
            framing: None,
            sig: Sig::Exit {
                code: 2,
                says: "the node start id must be at least 1, got -1",
            },
            reason: "renumber refuses a start ID below 1",
        }],
        ..Case::both(
            INDEXED,
            |w, dir, f| {
                let mut a = in_out(w, dir, f);
                a.push("--start-id=-1".into());
                a
            },
            |_, o| {
                let twin = mixed_sign_graph().relabelled_twin().0;
                let negated = |id: i64| -id;
                let g = MixedSignGraph {
                    nodes: twin
                        .nodes
                        .iter()
                        .map(|n| TestNode {
                            id: negated(n.id),
                            lat: n.lat,
                            lon: n.lon,
                            tags: n.tags.clone(),
                            meta: None,
                        })
                        .collect(),
                    ways: twin
                        .ways
                        .iter()
                        .map(|w| TestWay {
                            id: negated(w.id),
                            refs: w.refs.iter().map(|&r| negated(r)).collect(),
                            tags: w.tags.clone(),
                            meta: None,
                        })
                        .collect(),
                    relations: twin
                        .relations
                        .iter()
                        .map(|r| TestRelation {
                            id: negated(r.id),
                            members: r
                                .members
                                .iter()
                                .map(|m| crate::common::TestMember {
                                    id: match m.id {
                                        MemberId::Node(i) => MemberId::Node(-i),
                                        MemberId::Way(i) => MemberId::Way(-i),
                                        MemberId::Relation(i) => MemberId::Relation(-i),
                                        other => other,
                                    },
                                    role: m.role,
                                })
                                .collect(),
                            tags: r.tags.clone(),
                            meta: None,
                        })
                        .collect(),
                };
                // -1, -2, ... is canonical order already.
                pbf_result(o, OUT, &expect(&g, &ALL), true)
            },
        )
    }
}

// ---------------------------------------------------------------------------
// tags-filter
// ---------------------------------------------------------------------------

fn tags_filter_args(w: &World, dir: &Path, f: Framing, extra: &[&str]) -> Vec<OsString> {
    let mut a = in_out_forced(w, dir, f);
    a.extend(os(extra));
    a
}

/// `w/highway=primary` matches only `w-7`, which pulls in its nodes `-7`
/// and `7`.
fn tags_filter_way_dependencies() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| tags_filter_args(w, dir, f, &["w/highway=primary"]),
        |w, o| {
            let pick = Pick {
                nodes: Some(&[w.n(-7), w.n(7)]),
                ways: Some(&[w.w(-7)]),
                relations: Some(&[]),
                locations: false,
            };
            pbf_result(o, OUT, &expect(&w.base(), &pick), true)
        },
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-7 551000000,123000000 amenity=bench",
                "w-7 [-7,7] highway=primary",
            ],
            extra: &[],
        },
        "tags-filter does not select negative IDs",
    )])
}

/// `r/type=route` matches `r-1`, `r-7` and `r7`; the closure adds `r1`
/// (a member of `r-1`), every member way and every node of those ways.
fn tags_filter_relation_closure() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| tags_filter_args(w, dir, f, &["r/type=route"]),
        |w, o| {
            let nodes: Vec<i64> = [1, 2, 5, 7, -7, -5, -1, -2, 11]
                .iter()
                .map(|&i| w.n(i))
                .collect();
            let ways: Vec<i64> = [-2, 7, -1].iter().map(|&i| w.w(i)).collect();
            let relations: Vec<i64> = [-1, 1, -7, 7].iter().map(|&i| w.r(i)).collect();
            let pick = Pick {
                nodes: Some(&nodes),
                ways: Some(&ways),
                relations: Some(&relations),
                locations: false,
            };
            pbf_result(o, OUT, &expect(&w.base(), &pick), true)
        },
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-1 550100000,120100000 amenity=bench",
                "n-2 550200000,120000000 ",
                "n-5 551000000,121000000 ",
                "n-7 551000000,123000000 amenity=bench",
                "n1 550100000,120200000 amenity=bench",
                "n11 566000000,136000000 place=town",
                "n2 550200000,120300000 ",
                "n5 553000000,121000000 ",
                "n7 553000000,123000000 ",
                "w-1 [-1,-2,-5] highway=residential",
                "w-2 [1,2] highway=residential",
                "w7 [5,7,-7,-5,5] ",
                R_MINUS_1,
                R_MINUS_7,
                R_1,
            ],
            extra: &[],
        },
        "tags-filter does not select negative IDs",
    )])
}

/// `-R`: the direct match alone.
fn tags_filter_omit_referenced() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out(w, dir, f);
            a.extend(os(&["-R", "w/highway=primary"]));
            if !f.indexed() {
                a.extend(force(f));
            }
            a
        },
        |w, o| {
            let pick = Pick {
                nodes: Some(&[]),
                ways: Some(&[w.w(-7)]),
                relations: Some(&[]),
                locations: false,
            };
            pbf_result(o, OUT, &expect(&w.base(), &pick), true)
        },
    )
}

/// The actions of an OSC document as `action kind+id` tokens.
fn osc_tokens(text: &str) -> Vec<String> {
    let mut action = "";
    let mut out = Vec::new();
    for tag in text.split('<').skip(1) {
        let name: String = tag.chars().take_while(char::is_ascii_alphabetic).collect();
        match name.as_str() {
            "create" | "modify" | "delete" => {
                action = ["create", "modify", "delete"]
                    .into_iter()
                    .find(|a| *a == name)
                    .unwrap();
            }
            "node" | "way" | "relation" => {
                let attr = tag.find(" id=\"").map(|at| &tag[at + 5..]);
                if let Some(id) = attr.and_then(|rest| rest.split('"').next()) {
                    out.push(format!("{action} {}{id}", &name[..1]));
                }
            }
            _ => {}
        }
    }
    out
}

/// The bodies of an OSC's creates and modifies, `action kind+id body |
/// tags`: a node's coordinates in decimicrodegrees, a way's refs, a
/// relation's members, then the tags, each list in document order.
fn osc_bodies(text: &str) -> Vec<String> {
    fn attr<'t>(tag: &'t str, key: &str) -> Option<&'t str> {
        let at = tag.find(&format!(" {key}=\""))? + key.len() + 3;
        tag[at..].split('"').next()
    }
    let e7 = |v: &str| -> i64 {
        // Decimal degrees with at most seven places, read exactly.
        let (neg, v) = v.strip_prefix('-').map_or((false, v), |r| (true, r));
        let (int, frac) = v.split_once('.').unwrap_or((v, ""));
        let frac = format!("{frac:0<7}");
        let n: i64 = int.parse::<i64>().unwrap() * 10_000_000 + frac[..7].parse::<i64>().unwrap();
        if neg { -n } else { n }
    };
    let mut action = "";
    let mut out: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
    let mut open = false;
    for tag in text.split('<').skip(1) {
        let name: String = tag.chars().take_while(char::is_ascii_alphabetic).collect();
        match name.as_str() {
            "create" | "modify" | "delete" => {
                action = ["create", "modify", "delete"]
                    .into_iter()
                    .find(|a| *a == name)
                    .unwrap();
            }
            "node" | "way" | "relation" => {
                open = action != "delete";
                if open {
                    let mut head = format!("{action} {}{}", &name[..1], attr(tag, "id").unwrap());
                    if let (Some(lat), Some(lon)) = (attr(tag, "lat"), attr(tag, "lon")) {
                        head.push_str(&format!(" {},{}", e7(lat), e7(lon)));
                    }
                    out.push((head, Vec::new(), Vec::new()));
                }
            }
            "nd" if open => {
                out.last_mut()
                    .unwrap()
                    .1
                    .push(attr(tag, "ref").unwrap().to_owned());
            }
            "member" if open => {
                let kind = &attr(tag, "type").unwrap()[..1];
                let entry = format!(
                    "{kind}{}:{}",
                    attr(tag, "ref").unwrap(),
                    attr(tag, "role").unwrap()
                );
                out.last_mut().unwrap().1.push(entry);
            }
            "tag" if open => {
                let entry = format!("{}={}", attr(tag, "k").unwrap(), attr(tag, "v").unwrap());
                out.last_mut().unwrap().2.push(entry);
            }
            _ => {}
        }
    }
    out.into_iter()
        .map(|(head, body, tags)| format!("{head} {} | {}", body.join(","), tags.join(";")))
        .collect()
}

/// [`osc_bodies`]' token for `action` of element `kind`/`id` of `g`.
fn body_token(action: &str, g: &MixedSignGraph, kind: char, id: i64) -> String {
    match kind {
        'n' => {
            let n = g.node(id);
            format!(
                "{action} n{id} {},{}  | {}",
                n.lat,
                n.lon,
                tags_token(&n.tags)
            )
        }
        'w' => {
            let w = g.way(id);
            let refs: Vec<String> = w.refs.iter().map(i64::to_string).collect();
            format!(
                "{action} w{id} {} | {}",
                refs.join(","),
                tags_token(&w.tags)
            )
        }
        _ => {
            let r = g.relation(id);
            let members: Vec<String> = r
                .members
                .iter()
                .map(|m| member_token(m.id, m.role))
                .collect();
            format!(
                "{action} r{id} {} | {}",
                members.join(","),
                tags_token(&r.tags)
            )
        }
    }
}

fn osc_input(w: &World, dir: &Path, name: &str) -> OsString {
    let path = dir.join(name);
    std::fs::write(&path, osc_between(&w.base(), &w.graph(changed_graph()))).unwrap();
    path.into_os_string()
}

/// OSC mode keeps every delete and the modifies that match.
fn tags_filter_osc() -> Case {
    Case::both(
        INDEXED,
        |w, dir, _| {
            vec![
                osc_input(w, dir, "in.osc"),
                "-o".into(),
                out_arg(dir, "out.osc"),
                "r/route=tram".into(),
            ]
        },
        |w, o| {
            let (mut findings, path) = artifact(o, "out.osc", 0);
            if let Some(path) = path {
                let want = vec![
                    format!("modify r{}", w.r(-7)),
                    format!("delete w{}", w.w(-9)),
                ];
                let text = std::fs::read_to_string(path).unwrap();
                findings.extend(compare("actions", &want, &osc_tokens(&text), false));
                let bodies = vec![body_token(
                    "modify",
                    &w.graph(changed_graph()),
                    'r',
                    w.r(-7),
                )];
                findings.extend(compare("bodies", &bodies, &osc_bodies(&text), false));
            }
            findings
        },
    )
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// `type/id lat,lon;...` per GeoJSON feature on stdout.
fn feature_tokens(text: &str) -> Vec<String> {
    fn coords(v: &serde_json::Value, out: &mut Vec<String>) {
        match v.as_array() {
            Some(a) if a.len() == 2 && a.iter().all(serde_json::Value::is_number) => {
                // Coordinates in degrees; a decimicrodegree fits an i64.
                #[allow(clippy::cast_possible_truncation)]
                let e7 = |x: &serde_json::Value| (x.as_f64().unwrap() * 1e7).round() as i64;
                out.push(format!("{},{}", e7(&a[1]), e7(&a[0])));
            }
            Some(a) => a.iter().for_each(|x| coords(x, out)),
            None => {}
        }
    }
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: serde_json::Value = serde_json::from_str(line).unwrap();
            let p = &f["properties"];
            let id = p["@id"].to_string().trim_matches('"').to_owned();
            let kind = p["@type"].as_str().unwrap_or("?").to_owned();
            let mut c = Vec::new();
            coords(&f["geometry"]["coordinates"], &mut c);
            format!("{kind}/{id} {}", c.join(";"))
        })
        .collect()
}

fn export_nodes() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![base_input(w, dir, f), "--type".into(), "node".into()],
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            let g = w.base();
            let want: Vec<String> = [-1, -7, 1, 11]
                .iter()
                .map(|&i| {
                    let n = g.node(w.n(i));
                    format!("node/{} {},{}", n.id, n.lat, n.lon)
                })
                .collect();
            if o.code == Some(0) {
                findings.extend(compare(
                    "features",
                    &want,
                    &feature_tokens(&o.stdout),
                    false,
                ));
            }
            findings
        },
    )
}

fn export_ways() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let layout = Layout {
                locations: true,
                ..Layout::plain(f.indexed())
            };
            vec![
                write(dir, IN, &w.base(), layout),
                "--type".into(),
                "way".into(),
            ]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            let g = w.base();
            let want: Vec<String> = [-1, -2, -7, 1, 2, 9]
                .iter()
                .map(|&i| {
                    let way = g.way(w.w(i));
                    let c: Vec<String> = way
                        .refs
                        .iter()
                        .map(|r| {
                            let n = g.node(*r);
                            format!("{},{}", n.lat, n.lon)
                        })
                        .collect();
                    format!("way/{} {}", way.id, c.join(";"))
                })
                .collect();
            if o.code == Some(0) {
                findings.extend(compare(
                    "features",
                    &want,
                    &feature_tokens(&o.stdout),
                    false,
                ));
            }
            findings
        },
    )
}

// ---------------------------------------------------------------------------
// diff, digest, apply-changes, merge-changes
// ---------------------------------------------------------------------------

fn base_and_changed(w: &World, dir: &Path, f: Framing) -> (OsString, OsString) {
    let layout = Layout::plain(f.indexed());
    (
        write(dir, "old.osm.pbf", &w.base(), layout),
        write(dir, "new.osm.pbf", &w.graph(changed_graph()), layout),
    )
}

/// The changes between the base and the changed graph.
fn changed_actions(w: &World) -> Vec<String> {
    vec![
        format!("modify n{}", w.n(-7)),
        format!("modify r{}", w.r(-7)),
        format!("delete w{}", w.w(-9)),
    ]
}

fn diff_text() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let (old, new) = base_and_changed(w, dir, f);
            vec!["-c".into(), old, new]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 1).into_iter().collect();
            let got: Vec<String> = o
                .stdout
                .lines()
                .filter_map(|l| {
                    let action = match l.chars().next()? {
                        '*' => "modify",
                        '-' => "delete",
                        '+' => "create",
                        _ => return None,
                    };
                    let id = l[1..].split_whitespace().next()?;
                    Some(format!("{action} {id}"))
                })
                .collect();
            findings.extend(compare("changes", &changed_actions(w), &got, false));
            findings
        },
    )
}

fn diff_osc() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let (old, new) = base_and_changed(w, dir, f);
            vec![
                "--format".into(),
                "osc".into(),
                old,
                new,
                "-o".into(),
                out_arg(dir, "out.osc"),
            ]
        },
        |w, o| {
            let (mut findings, path) = artifact(o, "out.osc", 1);
            if let Some(path) = path {
                let got = osc_tokens(&std::fs::read_to_string(&path).unwrap());
                findings.extend(compare("actions", &changed_actions(w), &got, false));
                // The bodies: the derived OSC applied to the old file gives
                // the new one. Supplementary, since a diff and an apply bug
                // can cancel; the actions above are judged on their own.
                let scratch = pbfhogg_test_scratch::scratch();
                let applied = scratch.child("applied.osm.pbf");
                let mut apply = CliInvoker::new(&scratch)
                    .arg("apply-changes")
                    .arg(o.file("old.osm.pbf"))
                    .arg(&path)
                    .arg("-o")
                    .arg(&applied);
                if !o.framing.indexed() {
                    apply = apply.arg("--force");
                }
                let out = apply.run();
                let roundtrip = Observed {
                    code: out.status.code(),
                    stdout: out.stdout_str(),
                    stderr: out.stderr_str(),
                    dir: scratch.path().to_path_buf(),
                    framing: o.framing,
                };
                findings.extend(pbf_result(
                    &roundtrip,
                    "applied.osm.pbf",
                    &expect(&w.graph(changed_graph()), &ALL),
                    true,
                ));
            }
            findings
        },
    )
}

/// The per-kind counts; that the digest is the same in both framings is
/// the encoding-independence the command promises, which a mishandled
/// negative ID in either read path would break.
fn digest_full() -> Case {
    Case::both(
        INDEXED,
        |w, dir, _| {
            write(dir, "plain.osm.pbf", &w.base(), Layout::plain(false));
            // One node, the same record under `n-7` and under `n7`: only a
            // digest that keeps the sign tells the files apart.
            let base = w.base();
            for (name, id) in [("minus.osm.pbf", w.n(-7)), ("plus.osm.pbf", w.n(7))] {
                let single = MixedSignGraph {
                    nodes: vec![TestNode {
                        id,
                        ..base.node(w.n(-7)).clone()
                    }],
                    ways: Vec::new(),
                    relations: Vec::new(),
                };
                write(dir, name, &single, Layout::plain(true));
            }
            vec![base_input(w, dir, Framing::Indexed)]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            let digest = |name: &str| {
                let scratch = pbfhogg_test_scratch::scratch();
                CliInvoker::new(&scratch)
                    .arg("digest")
                    .arg(o.file(name))
                    .assert_success()
                    .stdout_str()
            };
            let (minus, plus) = (digest("minus.osm.pbf"), digest("plus.osm.pbf"));
            let kind_line = |text: &str, kind: &str| {
                text.lines()
                    .find(|l| l.starts_with(&format!("{kind} ")))
                    .map(str::to_owned)
            };
            let differs: Vec<String> = ["node", "way", "relation"]
                .into_iter()
                .filter(|k| kind_line(&minus, k) != kind_line(&plus, k))
                .map(str::to_owned)
                .collect();
            findings.extend(compare("sign", &["node".to_owned()], &differs, false));
            let g = w.base();
            let got: Vec<String> = o
                .stdout
                .lines()
                .skip(1)
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    Some(format!("{} {}", it.next()?, it.next()?))
                })
                .collect();
            let want = vec![
                format!("node {}", g.nodes.len()),
                format!("way {}", g.ways.len()),
                format!("relation {}", g.relations.len()),
            ];
            findings.extend(compare("counts", &want, &got, true));
            let plain = digest("plain.osm.pbf");
            if plain != o.stdout {
                findings.push(finding(
                    "framing",
                    Discrepancy::Other(format!("indexed {}\nunindexed {plain}", o.stdout)),
                ));
            }
            findings
        },
    )
}

fn apply_changes_default() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = vec![
                base_input(w, dir, f),
                osc_input(w, dir, "change.osc"),
                "-o".into(),
                out_arg(dir, OUT),
            ];
            a.extend(force(f));
            a
        },
        |w, o| pbf_result(o, OUT, &expect(&w.graph(changed_graph()), &ALL), true),
    )
}

/// The reverse change onto the changed graph: the negative way `w-9` is
/// created, `n-7` moved back and `r-7` retagged back, giving the base.
fn apply_changes_create() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let changed = w.graph(changed_graph());
            let osc = dir.join("change.osc");
            std::fs::write(&osc, osc_between(&changed, &w.base())).unwrap();
            let mut a = vec![
                write(dir, IN, &changed, Layout::plain(f.indexed())),
                osc.into_os_string(),
                "-o".into(),
                out_arg(dir, OUT),
            ];
            a.extend(force(f));
            a
        },
        judge_all,
    )
}

/// A moved negative node refreshes the inline locations of the ways over
/// it (`w-7` and `w7`).
fn apply_changes_locations_on_ways() -> Case {
    Case::both(
        INDEXED,
        |w, dir, _| {
            let layout = Layout {
                locations: true,
                ..Layout::plain(true)
            };
            vec![
                write(dir, IN, &w.base(), layout),
                osc_input(w, dir, "change.osc"),
                "-o".into(),
                out_arg(dir, OUT),
                "--locations-on-ways".into(),
            ]
        },
        |w, o| {
            let pick = Pick {
                locations: true,
                ..ALL
            };
            pbf_result(o, OUT, &expect(&w.graph(changed_graph()), &pick), true)
        },
    )
}

/// Two OSCs: the first moves `n-7`, the second moves `n-7` again and its
/// magnitude twin `n7`. Simplified, each signed key has one action: the
/// twins stay apart and `n-7`'s two changes collapse.
fn merge_changes_simplify() -> Case {
    Case::both(
        INDEXED,
        |w, dir, _| {
            let first = osc_input(w, dir, "a.osc");
            let second = dir.join("b.osc");
            std::fs::write(&second, osc_between(&w.graph(changed_graph()), &merged(w))).unwrap();
            vec![
                first,
                second.into_os_string(),
                "--simplify".into(),
                "-o".into(),
                out_arg(dir, "out.osc"),
            ]
        },
        |w, o| {
            let (mut findings, path) = artifact(o, "out.osc", 0);
            if let Some(path) = path {
                let mut want = changed_actions(w);
                want.push(format!("modify n{}", w.n(7)));
                let text = std::fs::read_to_string(path).unwrap();
                findings.extend(compare("actions", &want, &osc_tokens(&text), false));
                // The last change of each key: `n-7` at its second position.
                let g = merged(w);
                let bodies = vec![
                    body_token("modify", &g, 'n', w.n(-7)),
                    body_token("modify", &g, 'n', w.n(7)),
                    body_token("modify", &g, 'r', w.r(-7)),
                ];
                findings.extend(compare("bodies", &bodies, &osc_bodies(&text), false));
            }
            findings
        },
    )
}

/// The changed graph with `n-7` moved again and `n7` moved once.
fn merged(w: &World) -> MixedSignGraph {
    let mut g = w.graph(changed_graph());
    for n in &mut g.nodes {
        if n.id == w.n(7) || n.id == w.n(-7) {
            n.lat += 100_000;
        }
    }
    g
}

// ---------------------------------------------------------------------------
// getid and getparents
// ---------------------------------------------------------------------------

/// The query `n-7 w-1 r-7` in the arm's IDs, typed.
fn query(w: &World) -> Vec<String> {
    vec![
        format!("n{}", w.n(-7)),
        format!("w{}", w.w(-1)),
        format!("r{}", w.r(-7)),
    ]
}

fn picked(w: &World, nodes: &[i64], ways: &[i64], relations: &[i64]) -> Vec<String> {
    let nodes: Vec<i64> = nodes.iter().map(|&i| w.n(i)).collect();
    let ways: Vec<i64> = ways.iter().map(|&i| w.w(i)).collect();
    let relations: Vec<i64> = relations.iter().map(|&i| w.r(i)).collect();
    expect(
        &w.base(),
        &Pick {
            nodes: Some(&nodes),
            ways: Some(&ways),
            relations: Some(&relations),
            locations: false,
        },
    )
}

/// Every element but the picked ones.
fn all_but(w: &World, nodes: &[i64], ways: &[i64]) -> Vec<String> {
    let g = w.base();
    let nodes: Vec<i64> = g
        .nodes
        .iter()
        .map(|n| n.id)
        .filter(|id| !nodes.iter().any(|&i| w.n(i) == *id))
        .collect();
    let ways: Vec<i64> = g
        .ways
        .iter()
        .map(|x| x.id)
        .filter(|id| !ways.iter().any(|&i| w.w(i) == *id))
        .collect();
    expect(
        &g,
        &Pick {
            nodes: Some(&nodes),
            ways: Some(&ways),
            relations: None,
            locations: false,
        },
    )
}

/// The text-query refusal, naming the query's `n-7`.
const GETID_TEXT_REFUSAL: Sig = Sig::Exit {
    code: 64,
    says: "getid requires non-negative input ids. Input contains node id -7.",
};

fn getid_positional() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.push("--".into());
            a.extend(query(w).into_iter().map(OsString::from));
            a
        },
        |w, o| pbf_result(o, OUT, &picked(w, &[-7], &[-1], &[-7]), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        GETID_TEXT_REFUSAL,
        "getid refuses negative text queries",
    )])
}

fn getid_id_file() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let ids = dir.join("ids.txt");
            std::fs::write(&ids, query(w).join("\n") + "\n").unwrap();
            let mut a = in_out_forced(w, dir, f);
            a.extend(["-i".into(), ids.into_os_string()]);
            a
        },
        |w, o| pbf_result(o, OUT, &picked(w, &[-7], &[-1], &[-7]), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            says: "getid requires non-negative input ids. Input contains node id -7.",
        },
        "getid refuses negative IDs in an ID file",
    )])
}

/// `-I` takes every ID of a PBF: here `n-7`, `n1`, `w-1` and `r-7`.
fn getid_id_osm_file() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let base = w.base();
            let source = MixedSignGraph {
                nodes: vec![base.node(w.n(-7)).clone(), base.node(w.n(1)).clone()],
                ways: vec![base.way(w.w(-1)).clone()],
                relations: vec![base.relation(w.r(-7)).clone()],
            };
            let source = write(dir, "ids.osm.pbf", &source, Layout::plain(true));
            let mut a = in_out_forced(w, dir, f);
            a.extend(["-I".into(), source]);
            a
        },
        |w, o| pbf_result(o, OUT, &picked(w, &[-7, 1], &[-1], &[-7]), true),
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-7 551000000,123000000 amenity=bench",
                "w-1 [-1,-2,-5] highway=residential",
                "r-7 [n-7:stop,w7:] type=route;route=bus",
            ],
            extra: &[],
        },
        "getid -I drops negative source IDs",
    )])
}

/// `-r` from `r9`: its members `r7`, `n0`, `n-1`, `n-2`; through `r7` the
/// relation `r-7` and way `w-1`; through `r-7` node `n-7` and way `w7`;
/// then every node of `w-1` and `w7`.
fn getid_add_referenced() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.extend(["-r".into(), format!("r{}", w.r(9)).into()]);
            a
        },
        |w, o| {
            pbf_result(
                o,
                OUT,
                &picked(w, &[0, -1, -2, -5, -7, 5, 7], &[-1, 7], &[-7, 7, 9]),
                true,
            )
        },
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-1 550100000,120100000 amenity=bench",
                "n-2 550200000,120000000 ",
                "n-5 551000000,121000000 ",
                "n-7 551000000,123000000 amenity=bench",
                "n5 553000000,121000000 ",
                "n7 553000000,123000000 ",
                "w-1 [-1,-2,-5] highway=residential",
                "w7 [5,7,-7,-5,5] ",
                R_MINUS_7,
            ],
            extra: &[],
        },
        "getid -r drops negative members and refs",
    )])
}

fn getid_invert_node() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.extend([
                "--invert".into(),
                "--".into(),
                format!("n{}", w.n(-7)).into(),
            ]);
            a
        },
        |w, o| pbf_result(o, OUT, &all_but(w, &[-7], &[]), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        GETID_TEXT_REFUSAL,
        "getid refuses negative text queries",
    )])
}

fn getid_invert_way() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out_forced(w, dir, f);
            a.extend([
                "--invert".into(),
                "--".into(),
                format!("w{}", w.w(-7)).into(),
            ]);
            a
        },
        |w, o| pbf_result(o, OUT, &all_but(w, &[], &[-7]), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 64,
            says: "getid requires non-negative input ids. Input contains way id -7.",
        },
        "getid refuses negative text queries",
    )])
}

/// `n-7`'s parents are `w-7`, `w7` and `r-7`; `n7` has no relation parent,
/// so an abs-folded query cannot pass.
fn getparents_negative_query() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out(w, dir, f);
            a.extend(["--".into(), format!("n{}", w.n(-7)).into()]);
            a
        },
        |w, o| pbf_result(o, OUT, &picked(w, &[], &[-7, 7], &[-7]), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        GETID_TEXT_REFUSAL,
        "getparents refuses negative text queries",
    )])
}

/// `n1`'s parents are the negative way `w-2`, the way `w1` and the negative
/// relation `r-1`.
fn getparents_negative_parents() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = in_out(w, dir, f);
            a.push(format!("n{}", w.n(1)).into());
            a
        },
        |w, o| pbf_result(o, OUT, &picked(w, &[], &[-2, 1], &[-1]), true),
    )
}

// ---------------------------------------------------------------------------
// extract
// ---------------------------------------------------------------------------

/// Latitudes 54.9 to 55.09: holds `0, -1, -2, -9, 1, 2, 9`; `-5` (on `w-1`)
/// and `-7` lie just north of it.
const REGION_A: [f64; 4] = [11.9, 54.9, 12.4, 55.09];
/// Holds `5` and `7`, not `-7`.
const REGION_B: [f64; 4] = [12.05, 55.25, 12.35, 55.35];

fn bbox_arg(b: [f64; 4]) -> OsString {
    format!("{},{},{},{}", b[0], b[1], b[2], b[3]).into()
}

fn extract_args(w: &World, dir: &Path, f: Framing, strategy: &[&str]) -> Vec<OsString> {
    let mut a = vec![
        base_input(w, dir, f),
        "-o".into(),
        out_arg(dir, OUT),
        "-b".into(),
        bbox_arg(REGION_A),
    ];
    a.extend(os(strategy));
    if !strategy.contains(&"--simple") {
        a.extend(force(f));
    }
    a
}

/// What `--simple` writes for region A.
const SIMPLE_NODES: [i64; 7] = [0, -1, -2, -9, 1, 2, 9];
const SIMPLE_WAYS: [i64; 5] = [-1, -2, -9, 1, 9];
const SIMPLE_RELATIONS: [i64; 4] = [-1, 7, 9, 11];

fn extract_simple() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| extract_args(w, dir, f, &["--simple"]),
        |w, o| {
            pbf_result(
                o,
                OUT,
                &picked(w, &SIMPLE_NODES, &SIMPLE_WAYS, &SIMPLE_RELATIONS),
                true,
            )
        },
    )
    .gaps(vec![
        // Indexed, the region-filled node blob `[0, -1, -2]` is copied whole.
        gap(
            "elements",
            Some(Framing::Indexed),
            Sig::Tokens {
                missing: &[
                    "n-9 550500000,120500000 ",
                    "w-1 [-1,-2,-5] highway=residential",
                    "w-2 [1,2] highway=residential",
                    "w-9 [0,-9,9] ",
                    R_MINUS_1,
                    R_7,
                    R_11,
                ],
                extra: &[],
            },
            EXTRACT_DROPS,
        ),
        gap(
            "elements",
            Some(Framing::Unindexed),
            Sig::Tokens {
                missing: &[
                    "n-1 550100000,120100000 amenity=bench",
                    "n-2 550200000,120000000 ",
                    "n-9 550500000,120500000 ",
                    "w-1 [-1,-2,-5] highway=residential",
                    "w-2 [1,2] highway=residential",
                    "w-9 [0,-9,9] ",
                    R_MINUS_1,
                    R_7,
                    R_11,
                ],
                extra: &[],
            },
            EXTRACT_DROPS,
        ),
    ])
}

const EXTRACT_DROPS: &str = "extract does not select negative IDs";
const R_MINUS_1: &str = "r-1 [n1:stop,w-2:,r1:] type=route;route=bus";
const R_1: &str = "r1 [w7:outer,n11:label,r-1:subarea] type=boundary;boundary=administrative;admin_level=4;name=Mixed";
const R_7: &str = "r7 [r-7:,w-1:] type=route;route=road";
const R_11: &str = "r11 [w-2:outer,n-11:label] type=multipolygon;landuse=grass";
const R_MINUS_7: &str = "r-7 [n-7:stop,w7:] type=route;route=bus";

/// Complete-ways adds `-5`, the outside node of the matched `w-1`, and
/// `r1`, the parent of the included `r-1`.
fn extract_complete_ways() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| extract_args(w, dir, f, &[]),
        |w, o| {
            pbf_result(
                o,
                OUT,
                &picked(
                    w,
                    &[0, -1, -2, -5, -9, 1, 2, 9],
                    &SIMPLE_WAYS,
                    &[-1, 1, 7, 9, 11],
                ),
                true,
            )
        },
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-1 550100000,120100000 amenity=bench",
                "n-2 550200000,120000000 ",
                "n-5 551000000,121000000 ",
                "n-9 550500000,120500000 ",
                "w-1 [-1,-2,-5] highway=residential",
                "w-2 [1,2] highway=residential",
                "w-9 [0,-9,9] ",
                R_MINUS_1,
                R_1,
                R_7,
                R_11,
            ],
            extra: &[],
        },
        EXTRACT_DROPS,
    )])
}

/// Smart adds `-11`, the outside label member of the matched multipolygon
/// `r11`.
fn extract_smart() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| extract_args(w, dir, f, &["--smart"]),
        |w, o| {
            pbf_result(
                o,
                OUT,
                &picked(
                    w,
                    &[0, -1, -2, -5, -9, -11, 1, 2, 9],
                    &SIMPLE_WAYS,
                    &[-1, 1, 7, 9, 11],
                ),
                true,
            )
        },
    )
    .gaps(vec![gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-1 550100000,120100000 amenity=bench",
                "n-2 550200000,120000000 ",
                "n-5 551000000,121000000 ",
                "n-9 550500000,120500000 ",
                "n-11 565000000,135000000 ",
                "w-1 [-1,-2,-5] highway=residential",
                "w-2 [1,2] highway=residential",
                "w-9 [0,-9,9] ",
                R_MINUS_1,
                R_1,
                R_7,
                R_11,
            ],
            extra: &[],
        },
        EXTRACT_DROPS,
    )])
}

/// Two regions in one pass whose membership differs between the magnitude
/// twins: `-7` is in neither, `7` only in B, and B's ways and relations
/// follow from that.
fn extract_config() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let config = dir.join("regions.json");
            let region = |out: &str, b: [f64; 4]| {
                format!(
                    r#"{{"output":"{out}","bbox":[{},{},{},{}]}}"#,
                    b[0], b[1], b[2], b[3]
                )
            };
            std::fs::write(
                &config,
                format!(
                    r#"{{"directory":"{}","extracts":[{},{}]}}"#,
                    dir.display(),
                    region("a.osm.pbf", REGION_A),
                    region("b.osm.pbf", REGION_B)
                ),
            )
            .unwrap();
            vec![
                base_input(w, dir, f),
                "--config".into(),
                config.into_os_string(),
                "--simple".into(),
            ]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            for (check, name, want) in [
                (
                    "region a",
                    "a.osm.pbf",
                    picked(w, &SIMPLE_NODES, &SIMPLE_WAYS, &SIMPLE_RELATIONS),
                ),
                (
                    "region b",
                    "b.osm.pbf",
                    picked(w, &[5, 7], &[-7, 7], &[-7, 1]),
                ),
            ] {
                if o.file(name).exists() {
                    findings.extend(compare(check, &want, &observe(&o.file(name)), true));
                } else if o.code == Some(0) {
                    findings.push(finding(
                        "artifact",
                        Discrepancy::Other(format!("exited 0 without publishing {name}")),
                    ));
                }
            }
            findings
        },
    )
    .gaps(vec![
        gap(
            "region b",
            None,
            Sig::Tokens {
                missing: &["w-7 [-7,7] highway=primary", R_MINUS_7],
                extra: &[],
            },
            EXTRACT_DROPS,
        ),
        gap(
            "region a",
            None,
            Sig::Tokens {
                missing: &[
                    "n-1 550100000,120100000 amenity=bench",
                    "n-2 550200000,120000000 ",
                    "n-9 550500000,120500000 ",
                    "w-1 [-1,-2,-5] highway=residential",
                    "w-2 [1,2] highway=residential",
                    "w-9 [0,-9,9] ",
                    R_MINUS_1,
                    R_7,
                    R_11,
                ],
                extra: &[],
            },
            EXTRACT_DROPS,
        ),
    ])
}

// ---------------------------------------------------------------------------
// add-locations-to-ways
// ---------------------------------------------------------------------------

/// What add-locations-to-ways keeps by default: every way with its
/// locations, every relation, and the nodes that are tagged or relation
/// members.
fn altw_expected(w: &World) -> Vec<String> {
    let nodes: Vec<i64> = [0, -1, -2, -7, -11, 1, 11]
        .iter()
        .map(|&i| w.n(i))
        .collect();
    expect(
        &w.base(),
        &Pick {
            nodes: Some(&nodes),
            ways: None,
            relations: None,
            locations: true,
        },
    )
}

fn altw_args(w: &World, dir: &Path, f: Framing, extra: &[&str]) -> Vec<OsString> {
    let mut a = in_out(w, dir, f);
    a.extend(os(extra));
    a
}

/// The mixed graph's ten way refs to negative nodes, every one of them a
/// node the input holds, refused as unresolvable.
const SPARSE_REFUSAL: Sig = Sig::Exit {
    code: 2,
    says: "10 way node references (each use of a node in each way counted) could not be \
           resolved to a node location",
};

fn altw_sparse() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = altw_args(w, dir, f, &["--index-type", "sparse"]);
            a.extend(force(f));
            a
        },
        |w, o| pbf_result(o, OUT, &altw_expected(w), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        SPARSE_REFUSAL,
        "the sparse index does not join negative node refs",
    )])
}

fn altw_external() -> Case {
    Case::both(
        INDEXED,
        |w, dir, f| altw_args(w, dir, f, &["--index-type", "external"]),
        |w, o| pbf_result(o, OUT, &altw_expected(w), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            says: "node blob indexdata range [min_id=-2, max_id=0] holds negative node ids, \
                   which the external index cannot join",
        },
        "the external index refuses node blobs below zero",
    )])
}

/// Unindexed with `--force`, auto routes to sparse whatever the host.
fn altw_auto_forced() -> Case {
    Case::both(
        &[Framing::Unindexed],
        |w, dir, f| {
            let mut a = altw_args(w, dir, f, &[]);
            a.extend(force(f));
            a
        },
        |w, o| pbf_result(o, OUT, &altw_expected(w), true),
    )
    .gaps(vec![gap(
        "exit",
        None,
        SPARSE_REFUSAL,
        "the sparse index does not join negative node refs",
    )])
}

/// With `--ignore-missing-nodes` the strict gate cannot hide the join: a
/// present negative node gets its own coordinates, never the placeholder,
/// and the untagged `n-2` survives as a member of `r9`.
fn altw_retention_and_present_refs() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = altw_args(
                w,
                dir,
                f,
                &["--index-type", "sparse", "--ignore-missing-nodes"],
            );
            a.extend(force(f));
            a
        },
        |w, o| pbf_result(o, OUT, &altw_expected(w), true),
    )
    .gaps(vec![altw_placeholder_gap()])
}

/// The sparse index's answer under `--ignore-missing-nodes` today: every
/// present negative ref written as the placeholder, and the negative member
/// nodes `n-2` and `n-11` not retained.
fn altw_placeholder_gap() -> Gap {
    gap(
        "elements",
        None,
        Sig::Tokens {
            missing: &[
                "n-2 550200000,120000000 ",
                "n-11 565000000,135000000 ",
                "w-1 [-1,-2,-5] @550100000,120100000;550200000,120000000;551000000,121000000 highway=residential",
                "w-7 [-7,7] @551000000,123000000;553000000,123000000 highway=primary",
                "w-9 [0,-9,9] @550000000,120000000;550500000,120500000;550600000,120600000 ",
                "w1 [-1,1] @550100000,120100000;550100000,120200000 highway=residential",
                "w2 [-11,11] @565000000,135000000;566000000,136000000 building=yes",
                "w7 [5,7,-7,-5,5] @553000000,121000000;553000000,123000000;551000000,123000000;551000000,121000000;553000000,121000000 ",
                "w9 [2,9,-2] @550200000,120300000;550600000,120600000;550200000,120000000 highway=service",
            ],
            extra: &[
                "w-1 [-1,-2,-5] @0,0;0,0;0,0 highway=residential",
                "w-7 [-7,7] @0,0;553000000,123000000 highway=primary",
                "w-9 [0,-9,9] @550000000,120000000;0,0;550600000,120600000 ",
                "w1 [-1,1] @0,0;550100000,120200000 highway=residential",
                "w2 [-11,11] @0,0;566000000,136000000 building=yes",
                "w7 [5,7,-7,-5,5] @553000000,121000000;553000000,123000000;0,0;0,0;553000000,121000000 ",
                "w9 [2,9,-2] @550200000,120300000;550600000,120600000;0,0 highway=service",
            ],
        },
        "the sparse index writes the placeholder for present negative refs and drops \
         negative relation members from retention",
    )
}

/// A ref the input has no record of is the only placeholder: `n-9` from
/// `w9` reads `(0, 0)` while its twin `n9` keeps its coordinates.
fn altw_true_misses() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let input = write(
                dir,
                IN,
                &w.graph(dangling_graph()),
                Layout::plain(f.indexed()),
            );
            let mut a = vec![
                input,
                "-o".into(),
                out_arg(dir, OUT),
                "--index-type".into(),
                "sparse".into(),
                "--ignore-missing-nodes".into(),
            ];
            a.extend(force(f));
            a
        },
        |w, o| {
            let g = w.graph(dangling_graph());
            pbf_result(
                o,
                OUT,
                &expect(
                    &g,
                    &Pick {
                        nodes: Some(&[]),
                        ways: None,
                        relations: None,
                        locations: true,
                    },
                ),
                true,
            )
        },
    )
}

/// `--inject-prepass`: the ways that are multipolygon or boundary members
/// (`w-2` of `r11`, `w7` of `r1`) are marked, and every ref position whose
/// node more than one ref position references (a ring's closing ref not
/// counted) is pinned. `--ignore-missing-nodes` keeps the strict gate from
/// hiding the prepass behind a refusal; the elements are judged as in
/// `altw_retention_and_present_refs`.
fn altw_inject_prepass() -> Case {
    Case::both(
        INDEXED,
        |w, dir, f| {
            altw_args(
                w,
                dir,
                f,
                &[
                    "--index-type",
                    "sparse",
                    "--inject-prepass",
                    "--ignore-missing-nodes",
                ],
            )
        },
        |w, o| {
            let mut findings = pbf_result(o, OUT, &altw_expected(w), true);
            let path = o.file(OUT);
            if !path.exists() {
                return findings;
            }
            let header = crate::common::read_header(&path);
            if !header.has_way_members_v1() || !header.has_shared_node_pins_v1() {
                findings.push(finding(
                    "features",
                    Discrepancy::Other("the output header declares no prepass feature".into()),
                ));
            }
            let g = w.base();
            let open = |r: &[i64]| -> usize {
                if r.len() > 1 && r.first() == r.last() {
                    r.len() - 1
                } else {
                    r.len()
                }
            };
            let mut uses: BTreeMap<i64, usize> = BTreeMap::new();
            for way in &g.ways {
                for r in &way.refs[..open(&way.refs)] {
                    *uses.entry(*r).or_default() += 1;
                }
            }
            let mut want = vec![
                format!("member w{}", w.w(-2)),
                format!("member w{}", w.w(7)),
            ];
            for way in &g.ways {
                for (i, r) in way.refs.iter().enumerate() {
                    if uses[r] > 1 {
                        want.push(format!("pin w{} @{i}", way.id));
                    }
                }
            }
            findings.extend(compare("prepass", &want, &prepass_tokens(&path), false));
            findings
        },
    )
    .gaps(vec![
        altw_placeholder_gap(),
        gap(
            "prepass",
            None,
            Sig::Tokens {
                missing: &[
                    "member w-2",
                    "pin w-1 @0",
                    "pin w-1 @1",
                    "pin w-1 @2",
                    "pin w-7 @0",
                    "pin w1 @0",
                    "pin w7 @2",
                    "pin w7 @3",
                    "pin w9 @2",
                ],
                extra: &[],
            },
            "the prepass drops negative member ways and pins no negative shared node",
        ),
    ])
}

/// The prepass fields of `path`: `member w..` per WayMembers bit, `pin w..
/// @i` per SharedNodePins bit, and a token for a blob whose member count is
/// not its way count.
fn prepass_tokens(path: &Path) -> Vec<String> {
    let bit = |bytes: &[u8], i: usize| bytes.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0);
    let mut got = Vec::new();
    let mut reader = BlobReader::from_path(path).unwrap();
    reader.set_parse_waymembers(true);
    for blob in reader {
        let blob = blob.unwrap();
        let members = blob.way_members().map(<[u8]>::to_vec);
        let member_count = blob.way_member_count();
        let BlobDecode::OsmData(block) = blob.decode().unwrap() else {
            continue;
        };
        let ways = block
            .elements()
            .filter(|e| matches!(e, Element::Way(_)))
            .count();
        if ways > 0 && member_count != Some(u32::try_from(ways).unwrap()) {
            got.push(format!("member count {member_count:?} for {ways} ways"));
        }
        let mut index = 0;
        for element in block.elements() {
            let Element::Way(way) = element else {
                continue;
            };
            if members.as_deref().is_some_and(|m| bit(m, index)) {
                got.push(format!("member w{}", way.id()));
            }
            if let Some(pins) = way.shared_node_pins() {
                for i in 0..way.refs().count() {
                    if bit(pins, i) {
                        got.push(format!("pin w{} @{i}", way.id()));
                    }
                }
            }
            index += 1;
        }
    }
    got
}

/// Without `--ignore-missing-nodes` a ref the input has no record of is
/// refused: `w9`'s one ref to the absent `n-9`.
fn altw_true_miss_refused() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = vec![
                write(
                    dir,
                    IN,
                    &w.graph(dangling_graph()),
                    Layout::plain(f.indexed()),
                ),
                "-o".into(),
                out_arg(dir, OUT),
                "--index-type".into(),
                "sparse".into(),
            ];
            a.extend(force(f));
            a
        },
        |_, o| {
            let mut findings: Vec<Finding> = exit(o, 2).into_iter().collect();
            let says = "1 way node references (each use of a node in each way counted) could not be resolved";
            if o.code == Some(2) && !o.stderr.contains(says) {
                findings.push(finding("refusal", Discrepancy::Other(o.stderr.clone())));
            }
            if o.file(OUT).exists() {
                findings.push(finding(
                    "artifact",
                    Discrepancy::Other("a refused run published its output".into()),
                ));
            }
            findings
        },
    )
}

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

/// Counts and numeric ID extrema, judged against the arm's own graph; the
/// ranges' monotonicity is numeric, so the mixed graph's canonical order is
/// not monotonic there while its twin's is.
fn inspect_report() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![base_input(w, dir, f), "--id-ranges".into(), "--json".into()],
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            if o.code != Some(0) {
                return findings;
            }
            let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
            let g = w.base();
            let kinds: [(&str, Vec<i64>); 3] = [
                ("nodes", g.nodes.iter().map(|n| n.id).collect()),
                ("ways", g.ways.iter().map(|x| x.id).collect()),
                ("relations", g.relations.iter().map(|r| r.id).collect()),
            ];
            let mut want = Vec::new();
            let mut got = Vec::new();
            for (kind, ids) in &kinds {
                let range = &v["id_ranges"][kind];
                want.push(format!(
                    "{kind} {} {} {} {}",
                    ids.len(),
                    ids.iter().min().unwrap(),
                    ids.iter().max().unwrap(),
                    ids.is_sorted_by(|a, b| a < b)
                ));
                got.push(format!(
                    "{kind} {} {} {} {}",
                    v["elements"][kind], range["min"], range["max"], range["monotonic"]
                ));
            }
            findings.extend(compare("report", &want, &got, true));
            findings
        },
    )
    .gaps(vec![gap(
        "report",
        Some(Framing::Indexed),
        Sig::Tokens {
            missing: &["relations 6 -7 11 false"],
            extra: &["relations 6 -7 11 true"],
        },
        "the index-only report judges monotonicity on blob ID ranges, which cannot \
         see the numeric descent inside the relation blob [-1, -7, 1]",
    )])
}

/// The full-decode report counts every element of every kind.
fn inspect_extended() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![base_input(w, dir, f), "--extended".into(), "--json".into()],
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            if o.code != Some(0) {
                return findings;
            }
            let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
            let g = w.base();
            let want = vec![format!(
                "{} {} {}",
                g.nodes.len(),
                g.ways.len(),
                g.relations.len()
            )];
            let e = &v["elements"];
            let got = vec![format!("{} {} {}", e["nodes"], e["ways"], e["relations"])];
            findings.extend(compare("counts", &want, &got, false));
            findings
        },
    )
}

/// Node statistics read every node, negative ones included.
fn inspect_nodes() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = vec![base_input(w, dir, f), "--nodes".into()];
            a.extend(force(f));
            a
        },
        |w, o| check_verdict(o, 0, &[format!("Node count: {}\n", w.base().nodes.len())]),
    )
}

fn inspect_indexed() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![base_input(w, dir, f), "--indexed".into()],
        |_, o| {
            exit(o, if o.framing.indexed() { 0 } else { 1 })
                .into_iter()
                .collect()
        },
    )
}

/// `--show n-7` prints node `-7`'s own record, not `n7`'s.
fn inspect_show() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            vec![
                base_input(w, dir, f),
                "--show".into(),
                format!("n{}", w.n(-7)).into(),
            ]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            let n = w.base().node(w.n(-7)).clone();
            for needle in [
                format!("{}", n.id),
                "\"amenity\"=\"bench\"".to_owned(),
                format!("{:.7}", f64::from(n.lon) / 1e7),
            ] {
                if !o.stdout.contains(&needle) {
                    findings.push(finding(
                        "record",
                        Discrepancy::Other(format!("no {needle:?} in:\n{}", o.stdout)),
                    ));
                }
            }
            findings
        },
    )
}

/// Key counts over every element, independent of IDs.
fn inspect_tags_counts() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![base_input(w, dir, f)],
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            let g = w.base();
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            let tags = g
                .nodes
                .iter()
                .flat_map(|n| n.tags.iter())
                .chain(g.ways.iter().flat_map(|x| x.tags.iter()))
                .chain(g.relations.iter().flat_map(|r| r.tags.iter()));
            for (k, _) in tags {
                *counts.entry(k).or_default() += 1;
            }
            let want: Vec<String> = counts
                .iter()
                .map(|(k, n)| format!("{n}\t\"{k}\""))
                .collect();
            let got: Vec<String> = o.stdout.lines().map(str::to_owned).collect();
            findings.extend(compare("rows", &want, &got, false));
            findings
        },
    )
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// `check ARGS` on `input` exits `code` with every needle in its output.
fn check_verdict(o: &Observed, code: i32, needles: &[String]) -> Vec<Finding> {
    let mut findings: Vec<Finding> = exit(o, code).into_iter().collect();
    if findings.is_empty() {
        let output = o.output();
        for n in needles {
            if !output.contains(n.as_str()) {
                findings.push(finding(
                    "report",
                    Discrepancy::Other(format!("no {n:?} in:\n{output}")),
                ));
            }
        }
    }
    findings
}

fn check_on_base(extra: &'static [&'static str]) -> fn(&World, &Path, Framing) -> Vec<OsString> {
    // Each case names its own flags; the closure cannot capture them, so
    // the flags ride in a table keyed by the slice.
    match extra {
        [] => |w, dir, f| vec![base_input(w, dir, f)],
        ["--ids"] => |w, dir, f| vec![base_input(w, dir, f), "--ids".into()],
        ["--ids", "--full"] => {
            |w, dir, f| vec![base_input(w, dir, f), "--ids".into(), "--full".into()]
        }
        ["--index"] => |w, dir, f| vec![base_input(w, dir, f), "--index".into()],
        other => panic!("no argv for check {other:?}"),
    }
}

const CHECK_LOCATIONS_REFUSAL: Sig = Sig::Exit {
    code: 2,
    says: "the location check does not check negative IDs",
};

/// What a completed ID check of the base graph says it read.
fn scanned(w: &World) -> String {
    let g = w.base();
    format!(
        "Scanned {} nodes, {} ways, {} relations",
        g.nodes.len(),
        g.ways.len(),
        g.relations.len()
    )
}

fn check_default() -> Case {
    Case::both(BOTH, check_on_base(&[]), |w, o| {
        check_verdict(o, 0, &[scanned(w)])
    })
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            // Which negative element the parallel phases meet first varies.
            says: "has a negative ID; the reference check does not check negative IDs",
        },
        "check --refs refuses negative IDs",
    )])
}

fn check_ids() -> Case {
    Case::both(BOTH, check_on_base(&["--ids"]), |w, o| {
        check_verdict(o, 0, &[scanned(w)])
    })
}

fn check_ids_full() -> Case {
    Case::both(BOTH, check_on_base(&["--ids", "--full"]), |w, o| {
        check_verdict(o, 0, &[scanned(w)])
    })
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 1,
            says: "12 violations (showing first 12 of 12):
  node -1: outside the duplicate-detection ID range
  node -2: outside the duplicate-detection ID range
  node -5: outside the duplicate-detection ID range
  node -7: outside the duplicate-detection ID range
  node -9: outside the duplicate-detection ID range
  node -11: outside the duplicate-detection ID range
  way -1: outside the duplicate-detection ID range
  way -2: outside the duplicate-detection ID range
  way -7: outside the duplicate-detection ID range
  way -9: outside the duplicate-detection ID range
  relation -1: outside the duplicate-detection ID range
  relation -7: outside the duplicate-detection ID range
",
        },
        "check --ids --full reports every negative ID as out of range",
    )])
}

/// A duplicated `n-7` record, adjacent so the order stays canonical: the
/// duplicate is the finding, named by its signed ID.
fn check_ids_full_duplicate() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut g = w.base();
            let at = g.nodes.iter().position(|n| n.id == w.n(-7)).unwrap();
            let dup = g.nodes[at].clone();
            g.nodes.insert(at, dup);
            vec![
                write(dir, IN, &g, Layout::plain(f.indexed())),
                "--ids".into(),
                "--full".into(),
                "--json".into(),
            ]
        },
        |w, o| {
            let mut findings: Vec<Finding> = exit(o, 1).into_iter().collect();
            if o.code == Some(1) {
                let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
                let mut got = Vec::new();
                violations(&v, &mut got);
                let n = w.n(-7);
                let want = vec![
                    format!("duplicate node {n}"),
                    format!("non_monotonic node {n}"),
                ];
                findings.extend(compare("violations", &want, &got, false));
            }
            findings
        },
    )
    .gaps(vec![gap(
        "violations",
        None,
        Sig::Tokens {
            missing: &["duplicate node -7"],
            extra: &[
                "id_out_of_range node -1",
                "id_out_of_range node -2",
                "id_out_of_range node -5",
                "id_out_of_range node -7 x2",
                "id_out_of_range node -9",
                "id_out_of_range node -11",
                "id_out_of_range way -1",
                "id_out_of_range way -2",
                "id_out_of_range way -7",
                "id_out_of_range way -9",
                "id_out_of_range relation -1",
                "id_out_of_range relation -7",
            ],
        },
        "check --ids --full reports every negative ID as out of range and so finds \
         no negative duplicate",
    )])
}

/// Every ID violation in a `check --json` report as `type kind id`.
fn violations(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(kind) = map.get("type").and_then(serde_json::Value::as_str)
                && map.contains_key("elem_type")
            {
                out.push(format!(
                    "{kind} {} {}",
                    map["elem_type"].as_str().unwrap_or("?"),
                    map.get("id").map_or("?".to_owned(), ToString::to_string)
                ));
            }
            map.values().for_each(|x| violations(x, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| violations(x, out)),
        _ => {}
    }
}

fn graph_check(g: &MixedSignGraph, dir: &Path, f: Framing, extra: &[&str]) -> Vec<OsString> {
    let mut a = vec![write(dir, IN, g, Layout::plain(f.indexed()))];
    a.extend(os(extra));
    a
}

/// The `--show-ids` lines (`n123 in w456`) of a ref check's report.
fn missing_refs(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| {
            let parts: Vec<&str> = l.split(' ').collect();
            parts.len() == 3 && parts[1] == "in"
        })
        .map(str::to_owned)
        .collect()
}

fn refs_verdict(o: &Observed, want: &[String]) -> Vec<Finding> {
    let mut findings: Vec<Finding> = exit(o, 1).into_iter().collect();
    if o.code == Some(1) {
        findings.extend(compare("missing", want, &missing_refs(&o.output()), false));
    }
    findings
}

/// `w9`'s ref to the absent `n-9` is missing, and only it: `n9`, present,
/// is not.
fn check_refs_dangling() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            graph_check(
                &w.graph(dangling_graph()),
                dir,
                f,
                &["--refs", "--show-ids"],
            )
        },
        |w, o| refs_verdict(o, &[format!("n{} in w{}", w.n(-9), w.w(9))]),
    )
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            says: "w9 references negative ID n-9; the reference check does not check",
        },
        "check --refs refuses negative IDs",
    )])
}

/// The dangling graph with `w9` complete: the relation member `n-1` of
/// `r9` is the only missing reference, so the relation path is reached.
fn check_refs_relations_dangling() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut g = dangling_graph();
            g.ways[0].refs = vec![1, 9];
            graph_check(
                &w.graph(g),
                dir,
                f,
                &["--refs", "--check-relations", "--show-ids"],
            )
        },
        |w, o| refs_verdict(o, &[format!("n{} in r{}", w.n(-1), w.r(9))]),
    )
    .gaps(vec![gap(
        "exit",
        None,
        Sig::Exit {
            code: 2,
            says: "r9 references negative ID n-1; the reference check does not check",
        },
        "check --refs refuses negative IDs",
    )])
}

fn locations_input(w: &World, dir: &Path, f: Framing) -> OsString {
    let layout = Layout {
        locations: true,
        ..Layout::plain(f.indexed())
    };
    write(dir, IN, &w.base(), layout)
}

fn check_locations() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| vec![locations_input(w, dir, f), "--locations".into()],
        |_, o| check_verdict(o, 0, &[]),
    )
    .gaps(vec![gap(
        "exit",
        None,
        CHECK_LOCATIONS_REFUSAL,
        "check --locations refuses negative way refs",
    )])
}

fn check_locations_agreement() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            vec![
                locations_input(w, dir, f),
                "--locations".into(),
                "--agreement".into(),
            ]
        },
        |_, o| check_verdict(o, 0, &[]),
    )
    .gaps(vec![gap(
        "exit",
        None,
        CHECK_LOCATIONS_REFUSAL,
        "check --locations refuses negative way refs",
    )])
}

fn check_index() -> Case {
    Case::both(BOTH, check_on_base(&["--index"]), |_, o| {
        check_verdict(o, 0, &[])
    })
}

// ---------------------------------------------------------------------------
// build-geocode-index
// ---------------------------------------------------------------------------

/// The boundary `r1` ("Mixed", level 4) has the closed outer ring `w7`
/// over `5, 7, -7, -5`: a point inside it answers "Mixed", one outside
/// answers no boundary.
fn build_geocode_index_admin() -> Case {
    Case::both(
        BOTH,
        |w, dir, f| {
            let mut a = vec![
                base_input(w, dir, f),
                "--output-dir".into(),
                out_arg(dir, "index"),
            ];
            if !f.indexed() {
                a.push("--allow-unindexed".into());
            }
            a
        },
        |_, o| {
            let mut findings: Vec<Finding> = exit(o, 0).into_iter().collect();
            if o.code != Some(0) {
                return findings;
            }
            let reader = pbfhogg::geocode_index::reader::Reader::open(&o.file("index")).unwrap();
            let mut got = Vec::new();
            for (label, lat, lon) in [("inside", 55.2, 12.2), ("outside", 56.4, 13.0)] {
                match reader.query(lat, lon) {
                    Ok(result) => {
                        for admin in &result.admin {
                            got.push(format!("{label} {} {}", admin.admin_level, admin.name));
                        }
                    }
                    Err(e) => got.push(format!("{label} error {e}")),
                }
            }
            findings.extend(compare(
                "admin",
                &["inside 4 Mixed".to_owned()],
                &got,
                false,
            ));
            findings
        },
    )
    .gaps(vec![gap(
        "admin",
        None,
        Sig::Tokens {
            missing: &["inside 4 Mixed"],
            extra: &[],
        },
        "build-geocode-index skips negative node refs of admin ways",
    )])
}
