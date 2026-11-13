//! Verify: merge-changes - pbfhogg `merge-changes`, default and
//! `--simplify`, against osmium's reading of its inputs and a model of the
//! documented merge, with `osmium merge-changes -s` as a cross-check of the
//! model.
//!
//! Every change file, input or output, is read as a change listing
//! ([`crate::verify_change_listing`]): osmium's OPL of each change joined by
//! position with brokkr's XML scan, which keeps what osmium drops (the
//! create or modify label, which libosmium's writer derives from the
//! version, and the metadata attributes as written). osmium's own
//! `merge-changes` cannot be the oracle: it sorts every change by type, ID,
//! version and timestamp in both modes, so its default mode throws away the
//! order pbfhogg promises, and its `-s` keeps the highest version (then the
//! latest timestamp, then the last given) where pbfhogg keeps the last
//! change by position under a version floor.
//!
//! - Default mode must be its inputs concatenated, record for record.
//! - `--simplify` must be the last change of each object by input position,
//!   in a create, a modify and a delete group, each in (kind, signed ID)
//!   order, and must refuse (exit 2 naming the object and both versions, no
//!   output published) when a change claims a version lower than one
//!   claimed earlier for its object, an unversioned change claiming
//!   nothing; pbfhogg reports the regression at the smallest position.
//!   `--allow-version-regression` drops the floor and keeps the positional
//!   survivors. The model is computed by an external sort of fixed-size
//!   records ([`crate::verify_extsort`]): one object's history at a time,
//!   whatever the inputs' size.
//! - The same sort predicts what `osmium merge-changes -s` keeps (highest
//!   version, then the later timestamp, then the later position), and
//!   osmium's output must be exactly that: it checks the reading and the
//!   model on every object, and the objects where the two rules pick
//!   different changes are counted. libosmium compares timestamps only when
//!   both are valid, which is no consistent order when one object mixes
//!   changes with and without one, so the cross-check runs only when no
//!   object does.
//!
//! A delete is held to its input body (tags, coordinates, refs, members)
//! like any other change, as pbfhogg and osmium write one; a deleted node
//! with no location is held to none, never 0 0; an empty metadata attribute
//! is held to be omitted. pbfhogg refuses a deleted node with half a
//! location or an unusable coordinate: such an input fails the arm on
//! pbfhogg's exit, and no case here asserts the refusal, which pbfhogg's
//! own tests cover.
//! Section boundaries are not compared. Cases: the inputs, the inputs
//! twice over (several inputs, the run that can spill), and fixtures brokkr
//! writes that make the precedence rules observable, each required to show
//! the property it exists for.

use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::osc::{OscAction, OscKind};
use crate::reference::ToolRun;
use crate::verify_change_listing::{self as listing, ListingFile, ListingReader};
use crate::verify_extsort::{Record, Sorter};

/// The first version regression a serial pass in input order meets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Regression {
    kind: OscKind,
    id: i64,
    version: i64,
    held: i64,
}

impl Regression {
    /// The part of pbfhogg's error that names it.
    fn needle(&self) -> String {
        format!(
            "{} {} carries version {}, which is lower than version {}",
            kind_name(self.kind),
            self.id,
            self.version,
            self.held
        )
    }
}

fn kind_name(kind: OscKind) -> &'static str {
    match kind {
        OscKind::Node => "node",
        OscKind::Way => "way",
        OscKind::Relation => "relation",
    }
}

fn kind_of(index: u8) -> OscKind {
    match index {
        0 => OscKind::Node,
        1 => OscKind::Way,
        _ => OscKind::Relation,
    }
}

fn action_index(action: OscAction) -> u8 {
    match action {
        OscAction::Create => 0,
        OscAction::Modify => 1,
        OscAction::Delete => 2,
    }
}

/// One change, keyed for grouping by object in osmium's order: kind, then
/// non-positive IDs before positive ones, each by magnitude, then position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ByObject {
    kind: u8,
    positive: bool,
    magnitude: u64,
    position: u64,
    id: i64,
    offset: u64,
    action: u8,
    version: Option<i64>,
    opl_version: i64,
    timestamp: Option<i64>,
}

fn put_opt(out: &mut [u8], value: Option<i64>) {
    out[0] = u8::from(value.is_some());
    out[1..9].copy_from_slice(&value.unwrap_or(0).to_be_bytes());
}

fn get_i64(b: &[u8]) -> i64 {
    let mut a = [0; 8];
    a.copy_from_slice(&b[..8]);
    i64::from_be_bytes(a)
}

fn get_u64(b: &[u8]) -> u64 {
    let mut a = [0; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_be_bytes(a)
}

fn get_opt(b: &[u8]) -> Option<i64> {
    (b[0] == 1).then(|| get_i64(&b[1..9]))
}

impl Record for ByObject {
    const SIZE: usize = 1 + 1 + 8 + 8 + 8 + 8 + 1 + 9 + 8 + 9;
    fn encode(&self, out: &mut [u8]) {
        out[0] = self.kind;
        out[1] = u8::from(self.positive);
        out[2..10].copy_from_slice(&self.magnitude.to_be_bytes());
        out[10..18].copy_from_slice(&self.position.to_be_bytes());
        out[18..26].copy_from_slice(&self.id.to_be_bytes());
        out[26..34].copy_from_slice(&self.offset.to_be_bytes());
        out[34] = self.action;
        put_opt(&mut out[35..44], self.version);
        out[44..52].copy_from_slice(&self.opl_version.to_be_bytes());
        put_opt(&mut out[52..61], self.timestamp);
    }
    fn decode(b: &[u8]) -> Self {
        Self {
            kind: b[0],
            positive: b[1] == 1,
            magnitude: get_u64(&b[2..10]),
            position: get_u64(&b[10..18]),
            id: get_i64(&b[18..26]),
            offset: get_u64(&b[26..34]),
            action: b[34],
            version: get_opt(&b[35..44]),
            opl_version: get_i64(&b[44..52]),
            timestamp: get_opt(&b[52..61]),
        }
    }
}

/// A survivor in pbfhogg's emitted order: action group, kind, signed ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Emitted {
    group: u8,
    kind: u8,
    id: i64,
    offset: u64,
}

impl Record for Emitted {
    const SIZE: usize = 18;
    fn encode(&self, out: &mut [u8]) {
        out[0] = self.group;
        out[1] = self.kind;
        out[2..10].copy_from_slice(&self.id.to_be_bytes());
        out[10..18].copy_from_slice(&self.offset.to_be_bytes());
    }
    fn decode(b: &[u8]) -> Self {
        Self {
            group: b[0],
            kind: b[1],
            id: get_i64(&b[2..10]),
            offset: get_u64(&b[10..18]),
        }
    }
}

/// What the model expects of one case's inputs.
struct Model {
    /// The `--simplify` output, as listing records.
    simplified: PathBuf,
    /// What `osmium merge-changes -s` keeps, as OPL lines.
    osmium: PathBuf,
    regression: Option<Regression>,
    changes: u64,
    objects: u64,
    /// Objects whose osmium survivor is another change than pbfhogg's.
    divergent: u64,
    /// Objects mixing changes with and without a timestamp.
    unordered: u64,
    /// Input deletes carrying a body, each compared in default mode.
    deletes_with_body: u64,
    /// Surviving deletes: with a body, relations with members, and nodes
    /// with no location.
    surviving_deletes_with_body: u64,
    surviving_relation_deletes_with_members: u64,
    surviving_unlocated_node_deletes: u64,
}

/// One object's history, folded change by change.
struct History {
    first: ByObject,
    last: ByObject,
    floor: Option<i64>,
    regressed: bool,
    best: ByObject,
    changes: u64,
    timed: u64,
}

impl History {
    fn new(change: ByObject) -> Self {
        Self {
            first: change,
            last: change,
            floor: change.version,
            regressed: false,
            best: change,
            changes: 1,
            timed: u64::from(change.timestamp.is_some()),
        }
    }

    fn same_object(&self, change: &ByObject) -> bool {
        (self.first.kind, self.first.id) == (change.kind, change.id)
    }

    /// Fold the next change by position; returns a regression it raises.
    fn push(&mut self, change: ByObject) -> Option<(u64, Regression)> {
        let mut raised = None;
        if !self.regressed
            && let Some(version) = change.version
        {
            match self.floor {
                Some(held) if version < held => {
                    self.regressed = true;
                    raised = Some((
                        change.position,
                        Regression {
                            kind: kind_of(change.kind),
                            id: change.id,
                            version,
                            held,
                        },
                    ));
                }
                _ => self.floor = Some(self.floor.map_or(version, |f| f.max(version))),
            }
        }
        // osmium's -s keeps the highest version, then the later timestamp
        // when both have one, then the later position.
        let best = &self.best;
        let replaces = match change.opl_version.cmp(&best.opl_version) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => match (change.timestamp, best.timestamp) {
                (Some(c), Some(b)) => c >= b,
                _ => true,
            },
        };
        if replaces {
            self.best = change;
        }
        self.last = change;
        self.changes += 1;
        self.timed += u64::from(change.timestamp.is_some());
        raised
    }
}

/// Every change of `concatenated` keyed by object, sorted; with the change
/// count and the count of deletes carrying a body.
fn by_object(
    concatenated: &Path,
    dir: &Path,
) -> Result<(crate::verify_extsort::Sorted<ByObject>, u64, u64), Error> {
    let mut by_object = Sorter::<ByObject>::new(dir, "by-object");
    let mut reader = ListingReader::open(concatenated)?;
    let mut changes: u64 = 0;
    let mut deletes_with_body: u64 = 0;
    while let Some((offset, change)) = reader.next_record()? {
        if change.action == OscAction::Delete && change.has_body() {
            deletes_with_body += 1;
        }
        by_object.push(ByObject {
            kind: u8::try_from(change.kind.index()).unwrap_or(2),
            positive: change.id > 0,
            magnitude: change.id.unsigned_abs(),
            position: changes,
            id: change.id,
            offset,
            action: action_index(change.action),
            version: change.meta.version,
            opl_version: change.opl_version()?,
            timestamp: change.opl_timestamp()?,
        })?;
        changes += 1;
    }
    Ok((by_object.finish()?, changes, deletes_with_body))
}

/// Build the model of `concatenated` (the case's inputs as one listing).
fn model(concatenated: &Path, dir: &Path) -> Result<Model, Error> {
    let (mut sorted, changes, deletes_with_body) = by_object(concatenated, dir)?;
    let mut records = ListingFile::open(concatenated)?;
    let osmium = dir.join("expected-osmium-simplify.opl");
    let mut osmium_out = std::io::BufWriter::new(std::fs::File::create(&osmium)?);
    let mut emitted = Sorter::<Emitted>::new(dir, "emitted");
    let mut first_regression: Option<(u64, Regression)> = None;
    let (mut objects, mut divergent, mut unordered) = (0u64, 0u64, 0u64);

    let mut finish = |history: History,
                      records: &mut ListingFile,
                      emitted: &mut Sorter<Emitted>|
     -> Result<(), Error> {
        objects += 1;
        let last = history.last;
        emitted.push(Emitted {
            group: last.action,
            kind: last.kind,
            id: last.id,
            offset: last.offset,
        })?;
        if history.timed != 0 && history.timed != history.changes {
            unordered += 1;
        }
        if history.best.offset != last.offset {
            divergent += 1;
        }
        let line = records.at(history.best.offset)?.opl;
        std::io::Write::write_all(&mut osmium_out, line.as_bytes())?;
        std::io::Write::write_all(&mut osmium_out, b"\n")?;
        Ok(())
    };

    let mut current: Option<History> = None;
    while let Some(change) = sorted.next_record()? {
        match current.as_mut() {
            Some(history) if history.same_object(&change) => {
                if let Some((position, regression)) = history.push(change)
                    && first_regression
                        .as_ref()
                        .is_none_or(|(at, _)| position < *at)
                {
                    first_regression = Some((position, regression));
                }
            }
            _ => {
                if let Some(done) = current.replace(History::new(change)) {
                    finish(done, &mut records, &mut emitted)?;
                }
            }
        }
    }
    if let Some(done) = current.take() {
        finish(done, &mut records, &mut emitted)?;
    }
    sorted.remove()?;
    drop(finish);
    std::io::Write::flush(&mut osmium_out)?;

    let simplified = dir.join("expected-simplify.changes");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&simplified)?);
    let mut surviving_deletes_with_body = 0;
    let mut surviving_relation_deletes_with_members = 0;
    let mut surviving_unlocated_node_deletes = 0;
    let mut sorted = emitted.finish()?;
    while let Some(survivor) = sorted.next_record()? {
        let change = records.at(survivor.offset)?;
        if change.action == OscAction::Delete {
            surviving_deletes_with_body += u64::from(change.has_body());
            surviving_relation_deletes_with_members +=
                u64::from(change.kind == OscKind::Relation && change.has_field('M'));
            surviving_unlocated_node_deletes += u64::from(
                change.kind == OscKind::Node && !change.has_field('x') && !change.has_field('y'),
            );
        }
        std::io::Write::write_all(&mut out, change.record().as_bytes())?;
    }
    sorted.remove()?;
    std::io::Write::flush(&mut out)?;

    Ok(Model {
        simplified,
        osmium,
        regression: first_regression.map(|(_, r)| r),
        changes,
        objects,
        divergent,
        unordered,
        deletes_with_body,
        surviving_deletes_with_body,
        surviving_relation_deletes_with_members,
        surviving_unlocated_node_deletes,
    })
}

/// pbfhogg merge-changes of `inputs` to `out`, scratch declared exactly
/// where the run can spill.
fn argv(
    inputs: &[PathBuf],
    out: &Path,
    _dir: &Path,
    simplify: bool,
    allow_regression: bool,
    jobs: Option<usize>,
) -> Result<PbfArgv, Error> {
    let mut argv = PbfArgv::new("merge-changes");
    for input in inputs {
        argv = argv.input(input);
    }
    let mut argv = argv
        .output(out)
        .arg_if(simplify, "--simplify")
        .arg_if(allow_regression, "--allow-version-regression");
    if let Some(jobs) = jobs {
        argv = argv.arg("--jobs").arg(jobs.to_string());
    }
    Ok(if inputs.len() > 1 && !simplify && jobs != Some(1) {
        argv.scratch()
    } else {
        argv
    })
}

/// One case: the inputs (with their listings, in order) under every mode.
/// Returns its model; the osmium cross-check ran exactly when the model
/// counts no unordered object.
fn judge(
    harness: &mut Harness,
    dir: &Path,
    label: &str,
    inputs: &[PathBuf],
    listings: &[&Path],
) -> Result<Model, Error> {
    harness.msg(&format!("--- {label}: {} inputs ---", inputs.len()));
    let concatenated = dir.join("expected-default.changes");
    listing::concatenate(listings, &concatenated)?;
    let model = model(&concatenated, dir)?;
    harness.msg(&format!(
        "  {} changes of {} objects; regression: {}; osmium -s picks another change for {}",
        model.changes,
        model.objects,
        model
            .regression
            .as_ref()
            .map_or_else(|| "none".to_owned(), Regression::needle),
        model.divergent
    ));

    // Default mode: the inputs concatenated.
    let default_out = dir.join("default.osc.gz");
    let captured = harness.pbfhogg_discard(argv(inputs, &default_out, dir, false, false, None)?)?;
    harness.require_success(&captured, "pbfhogg merge-changes")?;
    let actual = listing::listing(harness, &default_out, dir, "default")?;
    let cmp = listing::compare(&concatenated, &actual.path)?;
    cmp.report(
        harness,
        &format!("{label}: default mode as the inputs concatenated"),
    );
    harness.record(
        &format!("{label}: merge-changes as its inputs concatenated"),
        Verdict::from_pass(cmp.is_equal()),
    );

    let serial_out = dir.join("default-j1.osc");
    let captured =
        harness.pbfhogg_discard(argv(inputs, &serial_out, dir, false, false, Some(1))?)?;
    harness.require_success(&captured, "pbfhogg merge-changes --jobs 1")?;
    match listing::same_content(&default_out, &serial_out)? {
        None => harness.msg(&format!("  {label}: --jobs 1 writes the same content PASS")),
        Some(why) => {
            harness.msg(&format!("  {label}: --jobs 1: {why} FAIL"));
            harness.fail(format!(
                "{label}: merge-changes --jobs 1 content differs: {why}"
            ));
        }
    }

    // --simplify, at the automatic and a single worker.
    for jobs in [None, Some(1)] {
        let name = match jobs {
            None => "simplify".to_owned(),
            Some(n) => format!("simplify-j{n}"),
        };
        let out = dir.join(format!("{name}.osc.gz"));
        let what = format!("{label}: merge-changes --simplify{}", jobs_flag(jobs));
        let args = argv(inputs, &out, dir, true, false, jobs)?;
        match &model.regression {
            Some(regression) => {
                expect_regression(harness, args, &out, regression, &what)?;
            }
            None => {
                let captured = harness.pbfhogg_discard(args)?;
                harness.require_success(&captured, &what)?;
                simplified_as_modelled(harness, &model, &out, dir, &name, &what)?;
            }
        }
    }
    if model.regression.is_some() {
        let out = dir.join("simplify-allow.osc.gz");
        let what = format!("{label}: merge-changes --simplify --allow-version-regression");
        let captured = harness.pbfhogg_discard(argv(inputs, &out, dir, true, true, None)?)?;
        harness.require_success(&captured, &what)?;
        simplified_as_modelled(harness, &model, &out, dir, "simplify-allow", &what)?;
    }

    // osmium's own -s, as the model predicts it.
    if model.unordered == 0 {
        let osmium_out = dir.join("osmium-simplify.opl");
        let mut run = ToolRun::osmium("merge-changes-simplify.1")
            .arg("merge-changes")
            .arg("-s");
        for input in inputs {
            run = run.input(input);
        }
        let run = run
            .args(&["-f", "opl", "-o"])
            .output(&osmium_out)
            .arg("--overwrite");
        let captured = harness.reference(&run, &dir.join("osmium-simplify.stdout"))?;
        harness.require_success(&captured, "osmium merge-changes -s")?;
        let cmp = listing::compare_lines(&model.osmium, &osmium_out)?;
        cmp.report(
            harness,
            &format!("{label}: osmium merge-changes -s as modelled"),
        );
        if !cmp.is_equal() {
            return Err(Error::Failed(format!(
                "{label}: osmium merge-changes -s keeps other changes than brokkr's model of \
                 it predicts, so the reading of the inputs cannot be trusted"
            )));
        }
    } else {
        harness.msg(&format!(
            "  {label}: osmium merge-changes -s not run: {} objects mix changes with and \
             without a timestamp",
            model.unordered
        ));
    }
    Ok(model)
}

fn jobs_flag(jobs: Option<usize>) -> String {
    jobs.map(|n| format!(" --jobs {n}")).unwrap_or_default()
}

fn simplified_as_modelled(
    harness: &mut Harness,
    model: &Model,
    out: &Path,
    dir: &Path,
    stem: &str,
    what: &str,
) -> Result<(), Error> {
    let actual = listing::listing(harness, out, dir, stem)?;
    let cmp = listing::compare(&model.simplified, &actual.path)?;
    cmp.report(
        harness,
        &format!("{what} as the last change per object by position"),
    );
    harness.record(what, Verdict::from_pass(cmp.is_equal()));
    Ok(())
}

/// The run `args` must refuse the regression: exit 2 naming it, and no
/// output at `out`.
fn expect_regression(
    harness: &mut Harness,
    args: PbfArgv,
    out: &Path,
    regression: &Regression,
    what: &str,
) -> Result<(), Error> {
    if listing::exists(out)? {
        return Err(Error::Failed(format!(
            "{} exists before the run that must not publish it",
            out.display()
        )));
    }
    let captured = harness.pbfhogg_discard(args)?;
    if listing::exists(out)? {
        harness.msg(&format!("  {what}: published {} FAIL", out.display()));
        harness.fail(format!(
            "{what} published an output over a version regression"
        ));
    }
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let needle = regression.needle();
    match captured.code() {
        Some(2) if stderr.contains(&needle) => {
            harness.msg(&format!("  {what}: refused {needle} PASS"));
        }
        Some(2) => {
            harness.msg(&format!(
                "  {what}: exit 2 without naming {needle} FAIL: {stderr}"
            ));
            harness.fail(format!(
                "{what} refused, but not naming the first regression ({needle})"
            ));
        }
        Some(0) => {
            harness.msg(&format!("  {what}: accepted {needle} FAIL"));
            harness.fail(format!("{what} accepted a version regression ({needle})"));
        }
        _ => harness.require_success(&captured, what)?,
    }
    Ok(())
}

/// A fixture: change files brokkr writes, and the property its model must
/// show for the fixture to test what it exists for.
struct Fixture {
    name: &'static str,
    files: &'static [&'static str],
    regression: Option<(&'static str, i64, i64, i64)>,
    divergent: bool,
    /// Surviving deletes with a body, a relation's with members among
    /// them, and a deleted node with no location.
    delete_bodies: bool,
}

const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\" generator=\"pbfhogg-verify\">\n";

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "precedence",
        files: &[
            "<modify>\n\
             <way id=\"5\" version=\"12\" timestamp=\"2024-01-01T00:00:00Z\" changeset=\"1\" uid=\"1\" user=\"a\"><nd ref=\"1\"/><nd ref=\"2\"/><tag k=\"highway\" v=\"primary\"/></way>\n\
             </modify>\n<delete>\n\
             <way id=\"6\" version=\"12\" timestamp=\"2024-01-01T00:00:00Z\" changeset=\"1\"><nd ref=\"1\"/><tag k=\"a\" v=\"b\"/></way>\n\
             </delete>\n<modify>\n\
             <node id=\"7\" version=\"3\" timestamp=\"2024-01-03T00:00:00Z\" changeset=\"2\" uid=\"2\" user=\"late\" lat=\"1\" lon=\"1\"><tag k=\"x\" v=\"1\"/></node>\n\
             <node id=\"8\" version=\"3\" timestamp=\"2024-01-01T00:00:00Z\" changeset=\"2\" uid=\"2\" user=\"early\" lat=\"1\" lon=\"1\"><tag k=\"x\" v=\"1\"/></node>\n\
             </modify>\n<create>\n\
             <node id=\"11\" version=\"5\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"2\" lon=\"2\"><tag k=\"shop\" v=\"bakery\"/></node>\n\
             <relation id=\"20\" version=\"1\" timestamp=\"2024-01-01T00:00:00Z\"><member type=\"node\" ref=\"11\" role=\"stop\"/><tag k=\"type\" v=\"route\"/></relation>\n\
             </create>\n",
            "<delete>\n\
             <way id=\"5\" version=\"12\" timestamp=\"2024-01-01T00:00:00Z\" changeset=\"3\"><nd ref=\"1\"/><tag k=\"highway\" v=\"primary\"/></way>\n\
             </delete>\n<modify>\n\
             <way id=\"6\" version=\"12\" timestamp=\"2024-01-01T00:00:00Z\" changeset=\"3\"><nd ref=\"1\"/><nd ref=\"3\"/><tag k=\"a\" v=\"c\"/></way>\n\
             <node id=\"7\" version=\"3\" timestamp=\"2024-01-02T00:00:00Z\" changeset=\"4\" uid=\"3\" user=\"early\" lat=\"1.5\" lon=\"1\"><tag k=\"x\" v=\"2\"/></node>\n\
             <node id=\"8\" version=\"3\" timestamp=\"2024-01-03T00:00:00Z\" changeset=\"4\" uid=\"3\" user=\"late\" lat=\"1.5\" lon=\"1\"><tag k=\"x\" v=\"2\"/></node>\n\
             </modify>\n<delete>\n\
             <node id=\"12\" version=\"3\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"3\" lon=\"3\"><tag k=\"amenity\" v=\"bench\"/></node>\n\
             <node id=\"13\" version=\"2\" timestamp=\"2024-01-01T00:00:00Z\"/>\n\
             <relation id=\"21\" version=\"4\" timestamp=\"2024-01-01T00:00:00Z\"><member type=\"way\" ref=\"6\" role=\"outer\"/><member type=\"node\" ref=\"12\" role=\"\"/><member type=\"relation\" ref=\"20\" role=\"sub\"/><tag k=\"type\" v=\"multipolygon\"/></relation>\n\
             </delete>\n",
        ],
        regression: None,
        divergent: true,
        delete_bodies: true,
    },
    Fixture {
        name: "backward",
        files: &[
            "<modify>\n<node id=\"1\" version=\"2\" timestamp=\"2024-01-02T00:00:00Z\" lat=\"1\" lon=\"1\"/>\n</modify>\n",
            "<modify>\n<node id=\"1\" version=\"1\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"2\" lon=\"2\"/>\n</modify>\n",
        ],
        regression: Some(("node", 1, 1, 2)),
        divergent: true,
        delete_bodies: false,
    },
    Fixture {
        name: "unversioned",
        files: &["<modify>\n\
             <node id=\"1\" version=\"9\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"1\" lon=\"1\"/>\n\
             <node id=\"1\" timestamp=\"2024-01-02T00:00:00Z\" lat=\"2\" lon=\"2\"/>\n\
             <node id=\"1\" version=\"8\" timestamp=\"2024-01-03T00:00:00Z\" lat=\"3\" lon=\"3\"/>\n\
             </modify>\n"],
        regression: Some(("node", 1, 8, 9)),
        divergent: true,
        delete_bodies: false,
    },
    Fixture {
        name: "first-regression",
        files: &[
            "<modify>\n\
             <node id=\"3\" version=\"5\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"1\" lon=\"1\"/>\n\
             <node id=\"9\" version=\"2\" timestamp=\"2024-01-01T00:00:00Z\" lat=\"1\" lon=\"1\"/>\n\
             </modify>\n",
            "<modify>\n\
             <node id=\"9\" version=\"1\" timestamp=\"2024-01-02T00:00:00Z\" lat=\"2\" lon=\"2\"/>\n\
             <node id=\"3\" version=\"4\" timestamp=\"2024-01-02T00:00:00Z\" lat=\"2\" lon=\"2\"/>\n\
             </modify>\n",
        ],
        regression: Some(("node", 9, 1, 2)),
        divergent: true,
        delete_bodies: false,
    },
];

/// Hold the model of a fixture to the property the fixture exists for.
fn fixture_shows(fixture: &Fixture, model: &Model) -> Result<(), Error> {
    let expected = fixture.regression.map(|(kind, id, version, held)| {
        format!("{kind} {id} carries version {version}, which is lower than version {held}")
    });
    let got = model.regression.as_ref().map(Regression::needle);
    let mut lacks = Vec::new();
    if got != expected {
        lacks.push(format!("regression {got:?}, not {expected:?}"));
    }
    if fixture.divergent && model.divergent == 0 {
        lacks.push("no object where osmium -s keeps another change".to_owned());
    }
    if fixture.delete_bodies {
        if model.surviving_deletes_with_body == 0 {
            lacks.push("no surviving delete with a body".to_owned());
        }
        if model.surviving_relation_deletes_with_members == 0 {
            lacks.push("no surviving relation delete with members".to_owned());
        }
        if model.surviving_unlocated_node_deletes == 0 {
            lacks.push("no surviving node delete without a location".to_owned());
        }
    }
    if lacks.is_empty() {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "fixture {} does not show what it exists for: {}",
            fixture.name,
            lacks.join("; ")
        )))
    }
}

/// Write `fixture`'s files under `outdir`, hold its model to the property it
/// exists for, then judge it as a case.
fn judge_fixture(harness: &mut Harness, outdir: &Path, fixture: &Fixture) -> Result<(), Error> {
    let dir = outdir.join(format!("fixture-{}", fixture.name));
    std::fs::create_dir(&dir)?;
    let mut files = Vec::new();
    let mut fixture_listings = Vec::new();
    for (i, body) in fixture.files.iter().enumerate() {
        let path = dir.join(format!("{}-{i}.osc", fixture.name));
        std::fs::write(&path, format!("{HEAD}{body}</osmChange>\n"))?;
        fixture_listings.push(listing::listing(harness, &path, &dir, &format!("in-{i}"))?);
        files.push(path);
    }
    let fixture_paths: Vec<&Path> = fixture_listings.iter().map(|l| l.path.as_path()).collect();
    let dir_case = dir.join("case");
    std::fs::create_dir(&dir_case)?;
    // The model alone first, so a fixture that lost its property is an
    // error before any pbfhogg verdict rests on it.
    let concatenated = dir_case.join("probe.changes");
    listing::concatenate(&fixture_paths, &concatenated)?;
    let probe_dir = dir.join("probe");
    std::fs::create_dir(&probe_dir)?;
    fixture_shows(fixture, &model(&concatenated, &probe_dir)?)?;
    let model = judge(
        harness,
        &dir_case,
        &format!("fixture {}", fixture.name),
        &files,
        &fixture_paths,
    )?;
    if model.unordered > 0 {
        return Err(Error::Failed(format!(
            "fixture {} mixes changes with and without a timestamp",
            fixture.name
        )));
    }
    Ok(())
}

/// Cross-validate pbfhogg merge-changes on the change files `oscs`, in
/// order, and on the fixtures.
pub fn run(harness: &mut Harness, oscs: &[PathBuf]) -> Result<Option<String>, Error> {
    if oscs.is_empty() {
        return Err(Error::Failed(
            "merge-changes needs at least one change file".into(),
        ));
    }
    let outdir = harness.subdir("merge-changes")?;
    harness.msg("=== verify merge-changes ===");
    for osc in oscs {
        harness.msg(&format!("  input: {}", osc.display()));
    }

    let inputs_dir = outdir.join("inputs");
    std::fs::create_dir(&inputs_dir)?;
    let mut listings = Vec::new();
    for (i, osc) in oscs.iter().enumerate() {
        listings.push(listing::listing(
            harness,
            osc,
            &inputs_dir,
            &format!("input-{i}"),
        )?);
    }
    let paths: Vec<&Path> = listings.iter().map(|l| l.path.as_path()).collect();

    let once = outdir.join("once");
    std::fs::create_dir(&once)?;
    let once_model = judge(harness, &once, "inputs", oscs, &paths)?;

    let twice_dir = outdir.join("twice");
    std::fs::create_dir(&twice_dir)?;
    let twice: Vec<PathBuf> = oscs.iter().chain(oscs).cloned().collect();
    let twice_paths: Vec<&Path> = paths.iter().chain(&paths).copied().collect();
    let twice_model = judge(harness, &twice_dir, "inputs twice", &twice, &twice_paths)?;

    for fixture in FIXTURES {
        judge_fixture(harness, &outdir, fixture)?;
    }

    let cross = if once_model.unordered == 0 {
        format!(
            "osmium merge-changes -s keeps exactly the changes brokkr's model of it predicts, \
             on the inputs, their replay and every fixture, its highest-version-then-timestamp \
             rule picking another change than pbfhogg's for {} of the inputs' {} objects",
            once_model.divergent, once_model.objects
        )
    } else {
        format!(
            "the osmium merge-changes -s cross-check ran on the fixtures only: {} input objects \
             mix changes with and without a timestamp, which libosmium does not order \
             consistently",
            once_model.unordered
        )
    };
    let twice_regresses = twice_model.regression.is_some();
    Ok(Some(format!(
        "on the inputs ({} change file{}) and the inputs twice over{}: default mode as its \
         inputs concatenated and --simplify as the last change per object by input position, \
         grouped create, modify, delete in (kind, ID) order, comparing action labels, metadata \
         attributes (an empty one held to be omitted) and osmium's decoding record for record, \
         a delete held to its input body ({} bodied input deletes checked, a deleted node \
         without a location held to none), section boundaries not compared, --jobs 1 as the automatic workers; on fixtures, \
         version regressions refused naming the first by position, unversioned changes \
         claiming nothing, --allow-version-regression and equal versions decided by position; \
         {cross}",
        oscs.len(),
        if oscs.len() == 1 { "" } else { "s" },
        if twice_regresses {
            ", where the replay is a version regression refused and then allowed"
        } else {
            ""
        },
        once_model.deletes_with_body
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn change(kind: u8, id: i64, position: u64, version: Option<i64>, ts: Option<i64>) -> ByObject {
        ByObject {
            kind,
            positive: id > 0,
            magnitude: id.unsigned_abs(),
            position,
            id,
            offset: position * 100,
            action: 1,
            version,
            opl_version: version.unwrap_or(0),
            timestamp: ts,
        }
    }

    fn fold(changes: &[ByObject]) -> (History, Option<(u64, Regression)>) {
        let mut history = History::new(changes[0]);
        let mut first = None;
        for c in &changes[1..] {
            let raised = history.push(*c);
            if first.is_none() {
                first = raised;
            }
        }
        (history, first)
    }

    #[test]
    fn merge_model_picks_first_regression_by_position_and_signed_survivor_order() {
        use crate::osc::OscMeta;
        use crate::verify_change_listing::Change;
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let changes = [
            (9, Some(9)),
            (1, Some(8)),
            (9, None),
            (9, Some(7)),
            (1, Some(6)),
        ]
        .map(|(id, version)| Change {
            action: OscAction::Modify,
            kind: OscKind::Node,
            id,
            meta: OscMeta {
                version,
                ..Default::default()
            },
            opl: format!("n{id} v{} x1 y2 T", version.unwrap_or(0)),
        });
        let input = dir.join("input.changes");
        std::fs::write(
            &input,
            changes.iter().map(Change::record).collect::<String>(),
        )
        .unwrap();
        let model = model(&input, dir).unwrap();
        assert_eq!(
            model.regression.unwrap().needle(),
            "node 9 carries version 7, which is lower than version 9"
        );
        assert_eq!(
            std::fs::read_to_string(model.simplified).unwrap(),
            format!("{}{}", changes[4].record(), changes[3].record())
        );
        assert_eq!((model.changes, model.objects, model.divergent), (5, 2, 2));
    }

    #[test]
    fn merge_argv_declares_scratch_only_for_spilling_shapes() {
        let dir = Path::new("private");
        let inputs = [PathBuf::from("one.osc"), PathBuf::from("two.osc")];
        for (count, simplify, jobs, spill) in [
            (1, false, None, false),
            (2, false, None, true),
            (2, false, Some(1), false),
            (2, true, None, false),
            (2, false, Some(0), true),
        ] {
            let (args, dirs) = argv(
                &inputs[..count],
                Path::new("out.osc"),
                dir,
                simplify,
                false,
                jobs,
            )
            .unwrap()
            .finish(dir);
            assert_eq!(args.iter().any(|a| a == "--tmp-dir"), spill);
            assert_eq!(dirs.len(), 1 + usize::from(spill));
            assert!(args.iter().any(|a| a == "--staging-dir"));
        }
    }

    #[test]
    fn records_round_trip() {
        let c = change(2, -7, 9, None, Some(-5));
        let mut buf = vec![0; ByObject::SIZE];
        c.encode(&mut buf);
        assert_eq!(ByObject::decode(&buf), c);
        let e = Emitted {
            group: 2,
            kind: 1,
            id: i64::MIN,
            offset: 3,
        };
        let mut buf = vec![0; Emitted::SIZE];
        e.encode(&mut buf);
        assert_eq!(Emitted::decode(&buf), e);
    }

    #[test]
    fn an_unversioned_change_neither_raises_nor_resets_the_floor() {
        let (h, raised) = fold(&[
            change(0, 1, 0, Some(9), Some(1)),
            change(0, 1, 1, None, Some(2)),
            change(0, 1, 2, Some(8), Some(3)),
        ]);
        let (_, regression) = raised.unwrap();
        assert_eq!((regression.version, regression.held), (8, 9));
        assert_eq!(h.last.position, 2);
        assert_eq!(h.best.position, 0, "osmium keeps v9");
    }

    #[test]
    fn equal_versions_go_by_position_and_osmium_by_timestamp() {
        let (h, raised) = fold(&[
            change(0, 7, 0, Some(3), Some(30)),
            change(0, 7, 1, Some(3), Some(20)),
        ]);
        assert!(raised.is_none());
        assert_eq!(h.last.position, 1);
        assert_eq!(h.best.position, 0);
        let (h, _) = fold(&[
            change(0, 8, 0, Some(3), Some(20)),
            change(0, 8, 1, Some(3), Some(20)),
        ]);
        assert_eq!(h.best.position, 1, "a full tie goes to the later change");
    }

    #[test]
    fn grouping_keys_keep_signed_identity_and_osmium_order() {
        let mut keys = [
            change(0, 5, 0, None, None),
            change(0, -2, 1, None, None),
            change(0, -10, 2, None, None),
            change(0, 0, 3, None, None),
            change(1, -1, 4, None, None),
        ];
        keys.sort();
        let ids: Vec<(u8, i64)> = keys.iter().map(|k| (k.kind, k.id)).collect();
        assert_eq!(ids, [(0, 0), (0, -2), (0, -10), (0, 5), (1, -1)]);
    }

    #[test]
    fn every_fixture_parses_as_one_document() {
        for fixture in FIXTURES {
            for (i, body) in fixture.files.iter().enumerate() {
                let scratch = pbfhogg_test_scratch::scratch();
                let path = scratch.path().join(format!("{}-{i}.osc", fixture.name));
                std::fs::write(&path, format!("{HEAD}{body}</osmChange>\n")).unwrap();
                let mut n = 0;
                crate::osc::scan_osc_changes(&path, |_| n += 1).unwrap();
                assert!(n > 0, "{}", fixture.name);
            }
        }
    }
}
