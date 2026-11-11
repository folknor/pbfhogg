//! The cancellation contract at the library level (ADR-0019).
//!
//! A command run under a `Host` whose `Invocation` token is tripped - before the
//! call, or at any checkpoint during it - returns `ErrorKind::Cancelled`,
//! publishes nothing, and leaves no output staging entry and no run scratch
//! behind. A publication whose decision was taken before the token tripped
//! completes, and its own outcome (an unsynced directory, say) stays
//! authoritative. Tokens are per invocation: tripping one stops only the runs
//! that observe it.
//!
//! The mid-run sweep trips the token from the `test-hooks` checkpoint hook at
//! sampled checkpoint ordinals in selected fixture scenarios, in process, so
//! "mid-run" needs no signal and no timing.

#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::path::{Path, PathBuf};

use pbfhogg_test_scratch::TestScratch;

/// One library command, run against fixtures in `inputs`, writing to `out`
/// and putting any scratch in `scratch`.
type Command = fn(&Inputs, &Path, &Path, &pbfhogg::host::Host) -> pbfhogg::Result<()>;

struct Inputs {
    dir: TestScratch,
}

impl Inputs {
    fn new() -> Self {
        let dir = crate::common::test_scratch();
        crate::common::write_base_pbf(&dir.path().join("base.osm.pbf"));
        crate::common::write_changed_pbf(&dir.path().join("changed.osm.pbf"));
        #[cfg(feature = "commands")]
        crate::common::write_synthetic_indexed_input(&dir.path().join("geo.osm.pbf"));
        std::fs::write(
            dir.path().join("change.osc"),
            b"<osmChange version=\"0.6\"><create>\
              <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
              <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn host_with(cancel: pbfhogg::host::Invocation) -> pbfhogg::host::Host {
    pbfhogg::host::Host {
        invocation: cancel,
        ..crate::common::test_host()
    }
}

fn sort(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::sort::sort(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn cat(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    let base = i.path("base.osm.pbf");
    pbfhogg::cat::cat(
        &[&base],
        crate::common::out(out.join("out.osm.pbf")),
        Some(pbfhogg::TypeFilter::all()),
        &Default::default(),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn altw(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
    index_type: pbfhogg::altw::IndexType,
) -> pbfhogg::Result<()> {
    pbfhogg::altw::add_locations_to_ways(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::altw::AltwOptions {
            keep_untagged_nodes: true,
            write: Default::default(),
            index_type,
            inject_prepass: false,
            ignore_missing_nodes: false,
        },
        &Default::default(),
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

fn altw_sparse(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    altw(i, out, scratch, host, pbfhogg::altw::IndexType::Sparse)
}

fn altw_external(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    altw(i, out, scratch, host, pbfhogg::altw::IndexType::External)
}

fn apply_changes(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::apply_changes::merge(
        &i.path("base.osm.pbf"),
        &i.path("change.osc"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::apply_changes::MergeOptions::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn merge_changes(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let osc = i.path("change.osc");
    // The second input spills its one member, and its file is closed,
    // before the first is read: every checkpoint of the run then falls at a
    // fixed point of the spill, so the sweep trips inside it and inside the
    // copy-back.
    #[cfg(feature = "test-hooks")]
    host.invocation
        .faults()
        .merge_changes_spill_first
        .store(1, std::sync::atomic::Ordering::SeqCst);
    pbfhogg::merge_changes::merge_changes(
        &[&osc, &osc],
        crate::common::out(out.join("merged.osc.gz")),
        pbfhogg::merge_changes::MergeMode::Stream {
            scratch: Some(pbfhogg::scratch::RunScratch::create(scratch, host)?),
        },
        pbfhogg::Parallelism::from_jobs(2),
        host,
    )
    .map(drop)
}

fn diff_parallel(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    // A text diff writes neither a file output nor scratch.
    let _ = (out, scratch);
    pbfhogg::diff::diff(
        &i.path("base.osm.pbf"),
        &i.path("changed.osm.pbf"),
        &mut Vec::new(),
        &pbfhogg::diff::DiffOptions {
            suppress_common: false,
            verbose: false,
            type_filter: None,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
        },
        false,
        host,
    )
    .map(drop)
}

fn derive_changes(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::diff::derive::derive_changes(
        &i.path("base.osm.pbf"),
        &i.path("changed.osm.pbf"),
        crate::common::out(out.join("delta.osc.gz")),
        false,
        false,
        None,
        pbfhogg::Parallelism::from_jobs(2),
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn geocode(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let output = pbfhogg::geocode_index::builder::index_output(
        out.join("index"),
        pbfhogg::Overwrite::Refuse,
        &host.diagnostics,
    )?;
    pbfhogg::geocode_index::builder::build_geocode_index(
        &pbfhogg::geocode_index::builder::BuildConfig {
            input_path: i.path("geo.osm.pbf"),
            allow_unindexed: true,
            ..Default::default()
        },
        output,
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

fn digest(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::digest::digest(
        &i.path("base.osm.pbf"),
        pbfhogg::digest::Form::Full,
        false,
        host,
    )
    .map(drop)
}

// Fixture scenarios, not a complete command inventory. This covers two altw
// modes and one mode each of the other listed commands. It omits getid,
// getparents, repack, renumber, tags-filter, tag counting (inspect tags),
// time-filter and other command modes. Ordinals identify callback positions,
// not source sites; parallel scheduling can change which position is reached.
fn commands() -> Vec<(&'static str, Command)> {
    let mut all: Vec<(&'static str, Command)> = vec![
        ("sort", sort),
        ("cat --type", cat),
        ("add-locations-to-ways sparse", altw_sparse),
        ("add-locations-to-ways external", altw_external),
        ("apply-changes", apply_changes),
        ("merge-changes", merge_changes),
        ("diff -j 2", diff_parallel),
        ("derive-changes -j 2", derive_changes),
        ("digest", digest),
    ];
    #[cfg(feature = "commands")]
    all.push(("build-geocode-index", geocode));
    all
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Run `command` once under `cancel`, in fresh output and scratch
/// directories, and check what is left: on `Cancelled`, nothing in either.
/// Returns whether the run was cancelled (it can complete when the token is
/// set to trip at a checkpoint the run never reaches).
fn run_once(
    name: &str,
    command: Command,
    inputs: &Inputs,
    cancel: pbfhogg::host::Invocation,
) -> Result<bool, String> {
    let out = crate::common::test_scratch();
    let scratch = crate::common::test_scratch();
    #[cfg(feature = "test-hooks")]
    let token = cancel.clone();
    let host = host_with(cancel);
    let result = command(inputs, out.path(), scratch.path(), &host);
    drop(host);
    // Every command, whatever its outcome, leaves its invocation with no
    // call guard, no permit and nothing in its census once it has returned
    // and everything it handed out is gone.
    #[cfg(feature = "test-hooks")]
    {
        let rest = token.capabilities();
        if rest != pbfhogg::host::Capabilities::default() {
            return Err(format!("{name}: capabilities left at rest: {rest:?}"));
        }
    }
    match result {
        Ok(()) => Ok(false),
        Err(e) if e.is_cancelled() => {
            if !entries(out.path()).is_empty() {
                return Err(format!(
                    "{name}: a cancelled run left {:?} in the output directory",
                    entries(out.path())
                ));
            }
            if !entries(scratch.path()).is_empty() {
                return Err(format!(
                    "{name}: a cancelled run left {:?} in the scratch parent",
                    entries(scratch.path())
                ));
            }
            Ok(true)
        }
        Err(e) => Err(format!(
            "{name}: expected Cancelled, got {:?}: {}",
            e.kind(),
            e.report()
        )),
    }
}

#[test]
fn a_tripped_token_stops_selected_scenarios_before_they_start() {
    let inputs = Inputs::new();
    let mut failures = Vec::new();
    for (name, command) in commands() {
        let cancel = pbfhogg::host::Invocation::new();
        cancel.cancel();
        match run_once(name, command, &inputs, cancel) {
            Ok(true) => {}
            Ok(false) => failures.push(format!("{name}: completed under a tripped token")),
            Err(e) => failures.push(e),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The result of targeting one ordinal of a fixture's counting run.
/// A missing callback is a coverage failure, not a successful cancellation.
#[cfg(feature = "test-hooks")]
#[derive(Debug)]
enum CheckpointRun {
    Unreached { observed: u64 },
    Cancelled,
    CompletedAfterDecision { authorized: u64 },
}

#[cfg(feature = "test-hooks")]
fn run_at_checkpoint(
    name: &str,
    command: Command,
    inputs: &Inputs,
    k: u64,
) -> Result<CheckpointRun, String> {
    let token = pbfhogg::host::Invocation::new();
    let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = std::sync::Arc::clone(&outcome);
    token.on_check(move |ordinal, token| {
        if ordinal == k {
            *seen.lock().unwrap() = Some(token.cancel());
        }
    });
    let cancelled = run_once(name, command, inputs, token.clone())?;
    let fired = *outcome.lock().unwrap();
    match (fired, cancelled) {
        (None, _) => Ok(CheckpointRun::Unreached {
            observed: token.checks_passed(),
        }),
        (Some(_), true) => Ok(CheckpointRun::Cancelled),
        (Some(pbfhogg::CancelOutcome::AfterDecision { authorized }), false) => {
            // Success is allowed only with evidence that publication had
            // already been authorized when this callback tripped the token.
            Ok(CheckpointRun::CompletedAfterDecision { authorized })
        }
        (Some(outcome), false) => Err(format!(
            "{name}: checkpoint {k} fired but the run completed after {outcome:?}"
        )),
    }
}

/// Sample fixture checkpoint ordinals for the selected command scenarios.
/// Each reached target must cancel or report an already decided publication.
#[cfg(feature = "test-hooks")]
#[test]
fn a_token_tripped_at_sampled_ordinals_cancels_or_follows_a_publication_decision() {
    let inputs = Inputs::new();
    let mut failures = Vec::new();
    for (name, command) in commands() {
        let counter = pbfhogg::host::Invocation::new();
        counter.on_check(|_, _| {});
        match run_once(name, command, &inputs, counter.clone()) {
            Ok(false) => {}
            Ok(true) => {
                failures.push(format!("{name}: counting run unexpectedly cancelled"));
                continue;
            }
            Err(e) => {
                failures.push(e);
                continue;
            }
        }
        let count = counter.checks_passed();
        if count == 0 {
            failures.push(format!("{name}: passed no checkpoint"));
            continue;
        }
        let mut cancelled = 0_u64;
        // Exhaust small fixture counts; larger counts use a spread and
        // always include the final ordinal. This is not site coverage.
        let mut ks: Vec<u64> = if count <= 64 {
            (1..=count).collect()
        } else {
            (1..=count)
                .step_by(usize::try_from(count / 64).unwrap_or(1))
                .collect()
        };
        if ks.last() != Some(&count) {
            ks.push(count);
        }
        for k in ks {
            match run_at_checkpoint(name, command, &inputs, k) {
                Ok(CheckpointRun::Cancelled) => cancelled += 1,
                Ok(CheckpointRun::CompletedAfterDecision { authorized }) => {
                    assert!(authorized > 0, "publication ordinals start at one");
                }
                Ok(CheckpointRun::Unreached { observed }) => failures.push(format!(
                    "{name}: target checkpoint {k} of {count} was not reached; observed {observed}"
                )),
                Err(e) => failures.push(format!("{e} (target checkpoint {k} of {count})")),
            }
        }
        if cancelled == 0 {
            failures.push(format!("{name}: no tripped checkpoint cancelled the run"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// At observed checkpoints of selected fixture scenarios, the census of
/// issued outputs, writers, readers and buffers is empty. This cannot rule
/// out a value issued and retired entirely between two observations.
#[cfg(feature = "test-hooks")]
#[test]
fn selected_scenarios_have_an_empty_census_at_observed_checkpoints() {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let inputs = Inputs::new();
    let mut failures = Vec::new();
    for (name, command) in commands() {
        let token = pbfhogg::host::Invocation::new();
        let issued = std::sync::Arc::new(AtomicU64::new(0));
        let seen = std::sync::Arc::clone(&issued);
        let observations = std::sync::Arc::new(AtomicU64::new(0));
        let observed = std::sync::Arc::clone(&observations);
        token.on_check(move |_, token| {
            observed.fetch_add(1, Relaxed);
            let c = token.capabilities();
            seen.fetch_max(c.session + c.writer + c.output + c.buffer, Relaxed);
        });
        match run_once(name, command, &inputs, token) {
            Ok(false) => {}
            Ok(true) => failures.push(format!("{name}: census run unexpectedly cancelled")),
            Err(e) => failures.push(e),
        }
        if observations.load(Relaxed) == 0 {
            failures.push(format!("{name}: census callback observed no checkpoints"));
        }
        if issued.load(Relaxed) != 0 {
            failures.push(format!("{name}: issued a value of its own to the census"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A public command reached from inside another (`sort` runs `sort_with`)
/// is a nested entry: it mints its own call guard while the outer one is
/// held, so a checkpoint inside it sees two, and both retire.
#[cfg(feature = "test-hooks")]
#[test]
fn a_nested_entry_mints_its_own_guard() {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let inputs = Inputs::new();
    let token = pbfhogg::host::Invocation::new();
    let at_first = std::sync::Arc::new(AtomicU64::new(0));
    let seen = std::sync::Arc::clone(&at_first);
    token.on_check(move |ordinal, token| {
        // The first checkpoint is sort_with's, before any writer (whose
        // methods are entries too) exists.
        if ordinal == 1 {
            seen.store(token.capabilities().guards, Relaxed);
        }
    });
    assert_eq!(run_once("sort", sort, &inputs, token.clone()), Ok(false));
    assert_eq!(at_first.load(Relaxed), 2, "sort and its nested sort_with");
    assert_eq!(token.capabilities(), pbfhogg::host::Capabilities::default());
}

/// Publication checks the token once, as its decision. A token tripped after
/// that (here, by a `before_publish` hook, which runs after the decision)
/// does not interrupt it.
#[test]
fn a_decided_publication_completes() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osm.pbf");
    let host = crate::common::test_host();
    let output = crate::common::out(&dest).bind(&host.invocation).unwrap();
    let mut writer = output
        .pbf_writer(
            Default::default(),
            &pbfhogg::block_builder::HeaderBuilder::new()
                .build()
                .unwrap(),
            pbfhogg::write::OutputBackend::Buffered,
            &host,
        )
        .unwrap();
    writer.flush().unwrap();
    drop(writer);
    let cancel = host.invocation.clone();
    let tripped = host.invocation.clone();
    let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = std::sync::Arc::clone(&outcome);
    output
        .before_publish(move || *seen.lock().unwrap() = Some(tripped.cancel()))
        .publish()
        .expect("a decided publication runs to completion");
    assert!(cancel.is_cancelled());
    assert!(dest.is_file());
    assert_eq!(
        *outcome.lock().unwrap(),
        Some(pbfhogg::CancelOutcome::AfterDecision { authorized: 1 })
    );
}

/// The decision refuses an output whose token was tripped before
/// publication: `Cancelled`, nothing installed, the staging entry gone.
#[test]
fn publication_refuses_once_the_token_is_tripped() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osc");
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(&dest).bind(&cancel).unwrap();
    let mut writer = output.writer().unwrap();
    std::io::Write::write_all(&mut writer, b"data").unwrap();
    writer.finish().unwrap();
    assert_eq!(
        cancel.cancel(),
        pbfhogg::CancelOutcome::BeforeDecision { refused: 1 }
    );
    let err = output.publish().expect_err("refused");
    assert!(err.is_cancelled(), "{err}");
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

/// A write through a cancelled `OutputWriter` fails the session, so even a
/// caller that ignores the error cannot publish what it wrote.
#[test]
fn a_cancelled_output_writer_cannot_be_published() {
    let dir = crate::common::test_scratch();
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(dir.path().join("out.txt"))
        .bind(&cancel)
        .unwrap();
    let writer = output.writer().unwrap();
    // The writer is no owner: tripping the token decides the output.
    cancel.cancel();
    let err = writer.finish().expect_err("finish observes the token");
    assert!(err.is_cancelled(), "{err}");
    let err = output
        .publish()
        .expect_err("a cancelled output is not published");
    assert!(err.is_cancelled(), "{err}");
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

/// After the decision, publication's own outcome stands: an unsynced
/// directory is reported as such, not turned into a cancellation.
#[cfg(feature = "test-hooks")]
#[test]
fn a_publication_failure_after_the_decision_is_not_a_cancellation() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osc");
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(&dest).bind(&cancel).unwrap();
    cancel
        .faults()
        .arm_publish_fault(pbfhogg::output::PublishFault::DirSync);
    let mut writer = output.writer().unwrap();
    std::io::Write::write_all(&mut writer, b"data").unwrap();
    writer.finish().unwrap();
    let tripped = cancel.clone();
    let err = output
        .before_publish(move || {
            tripped.cancel();
        })
        .publish()
        .expect_err("the injected directory sync failure");
    assert!(!err.is_cancelled(), "{err}");
    assert!(
        matches!(
            err.kind(),
            pbfhogg::ErrorKind::Output(pbfhogg::OutputError::Unsynced { .. })
        ),
        "{err}"
    );
    assert!(dest.is_file(), "the output was published");
}

/// Each invocation observes its own token: two sorts running at once, one
/// cancelled mid-run, the other completing.
#[cfg(feature = "test-hooks")]
#[test]
fn a_token_stops_only_the_runs_that_observe_it() {
    let inputs = Inputs::new();
    let stopped = pbfhogg::host::Invocation::new();
    stopped.on_check(|ordinal, token| {
        if ordinal == 3 {
            token.cancel();
        }
    });
    let (cancelled, completed) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run_once("sort a", sort, &inputs, stopped.clone()));
        let b = scope.spawn(|| run_once("sort b", sort, &inputs, pbfhogg::host::Invocation::new()));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(cancelled, Ok(true));
    assert_eq!(completed, Ok(false));
}
