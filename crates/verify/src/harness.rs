//! The state one verifier run threads through its checks, and how a check
//! turns into a result line.
//!
//! A check is a closure over `&mut Harness` returning its claim (what a PASS
//! establishes, when narrower than its name). Each comparison it makes is
//! recorded on the harness ([`Harness::record`]), not in a value the check
//! could drop: any recorded failure fails the check, whatever the closure
//! returns afterwards, and an environment error cannot erase a failure
//! recorded before it.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::opl_compare::{self, Policy};
use crate::output::{self, Detail};
use crate::process::{Ended, Launch, Lifecycle};
use crate::reference::{ToolRun, Tools};
use crate::run::Run;
use crate::verify_fileinfo::{self, FileInfo};
use crate::{pbf_frames, stream, tunables};

/// The outcome of one comparison inside a check.
#[must_use = "a discarded verdict is a silent PASS - record it on the harness"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    pub(crate) fn is_pass(self) -> bool {
        self == Self::Pass
    }

    pub(crate) fn from_pass(ok: bool) -> Self {
        if ok { Self::Pass } else { Self::Fail }
    }
}

/// What a command's documented header rule allows its output's
/// `LocationsOnWays` declaration to be on an input that declares it
/// ([`Harness::judge_way_locations`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WayLocations {
    /// Every way blob is copied whole, so the declaration is kept.
    Kept,
    /// Ways are re-encoded without coordinates: dropped, with the warning.
    Dropped,
    /// Kept, or dropped with the warning where the run decides (`sort`
    /// rewriting a way blob, `getid --invert` removing a way): the verifier
    /// cannot see which.
    KeptOrReported,
}

/// Whether `stderr` carries the CLI's loss warning naming `LocationsOnWays`
/// as one member of its list.
fn warns_locations_loss(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        line.strip_prefix("Warning: the input declares ")
            .and_then(|rest| rest.split_once(", which the output does not declare"))
            .is_some_and(|(list, _)| list.split(", ").any(|f| f == "LocationsOnWays"))
    })
}

/// How one check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckStatus {
    Passed,
    Failed,
    /// Not completed for a reason outside the code under test, with no
    /// comparison failure recorded before it.
    Environment,
}

/// Exit code of a run whose only non-passing checks could not be completed
/// for environmental reasons: `EX_TEMPFAIL`, distinct from 1, which always
/// means pbfhogg failed a check.
pub(crate) const ENVIRONMENT_EXIT_CODE: u8 = 75;

pub(crate) struct Harness {
    life: Lifecycle,
    cache: crate::reference_cache::ReferenceCache,
    pub(crate) run: Run,
    tools: Tools,
    detail: Detail,
    /// Comparison failures of the check in flight.
    failures: Vec<String>,
    /// Wall time per step label of the check in flight.
    steps: Vec<(String, Duration, usize)>,
    /// Numbers each launch's private directory within the run.
    launches: u64,
    /// The check in flight's directory.
    check_dir: Option<PathBuf>,
}

impl Harness {
    pub(crate) fn new(
        life: Lifecycle,
        run: Run,
        tools: Tools,
        verbose: bool,
        cache: crate::reference_cache::ReferenceCache,
    ) -> Self {
        Self {
            life,
            cache,
            run,
            tools,
            detail: Detail::new(verbose),
            failures: Vec::new(),
            steps: Vec::new(),
            launches: 0,
            check_dir: None,
        }
    }

    pub(crate) fn has_osmosis(&self) -> bool {
        self.tools.osmosis.is_some()
    }

    pub(crate) fn retain_references(&mut self, budget: u64) -> Result<(), Error> {
        self.checkpoint()?;
        self.cache.retain(Some(budget));
        Ok(())
    }

    pub(crate) fn checkpoint(&mut self) -> Result<(), Error> {
        self.life.checkpoint()
    }

    /// One line of check detail.
    pub(crate) fn msg(&mut self, line: &str) {
        self.detail.line(line);
    }

    /// Record one comparison's verdict under `what`.
    pub(crate) fn record(&mut self, what: &str, verdict: Verdict) {
        if verdict == Verdict::Fail {
            self.fail(what);
        }
    }

    /// Record an unconditional failure: a comparison that could not be made
    /// counts as failed, never as passed.
    pub(crate) fn fail(&mut self, what: impl Into<String>) {
        self.failures.push(what.into());
    }

    /// The fresh directory of the check in flight, named `name`.
    pub(crate) fn subdir(&mut self, name: &str) -> Result<PathBuf, Error> {
        let dir = self.run.check_dir(name)?;
        self.check_dir = Some(dir.clone());
        Ok(dir)
    }

    fn private_dir(&mut self, kind: &str) -> Result<PathBuf, Error> {
        self.launches += 1;
        let base = self
            .check_dir
            .clone()
            .unwrap_or_else(|| self.run.dir.clone());
        let dir = base.join(format!(".{kind}-{}", self.launches));
        std::fs::create_dir(&dir)?;
        Ok(dir)
    }

    fn step(&mut self, label: &str, took: Duration) {
        match self.steps.iter_mut().find(|(l, _, _)| l == label) {
            Some((_, total, runs)) => {
                *total += took;
                *runs += 1;
            }
            None => self.steps.push((label.to_owned(), took, 1)),
        }
    }

    /// Run the pbfhogg CLI under test with its standard output written to
    /// the file `sink`.
    /// The exit status is the caller's to judge.
    pub(crate) fn pbfhogg(&mut self, argv: PbfArgv, sink: &Path) -> Result<Ended, Error> {
        let label = format!("pbfhogg {}", argv.subcommand());
        let private = self.private_dir("pbfhogg")?;
        let (args, dirs) = argv.finish(&private);
        for dir in &dirs {
            std::fs::create_dir(dir)?;
        }
        let cwd = private.clone();
        let program = self.run.pbfhogg.clone();
        let started = Instant::now();
        let ended = self.life.run(&Launch {
            program: &program,
            args,
            env: Vec::new(),
            cwd: &cwd,
            sink,
        });
        self.step(&label, started.elapsed());
        ended
    }

    /// Run a step whose stdout is not a comparison listing, retaining it in
    /// a launch-private file for diagnosis.
    pub(crate) fn pbfhogg_discard(&mut self, argv: PbfArgv) -> Result<Ended, Error> {
        let dir = self.private_dir("stdout")?;
        self.pbfhogg(argv, &dir.join("stdout"))
    }

    /// Check inline locations against kept nodes and other references.
    pub(crate) fn check_locations(
        &mut self,
        label: &str,
        pbf: &Path,
        agreement: bool,
    ) -> Result<(Verdict, String), Error> {
        let sink = pbf.with_extension("locations.txt");
        let ended = self.pbfhogg(
            PbfArgv::new("check")
                .arg("--locations")
                .arg_if(agreement, "--agreement")
                .input(pbf),
            &sink,
        )?;
        let report = stream::read_report(&sink)?;
        match ended.code() {
            Some(0) => {
                self.msg(&format!("  {label}: inline locations consistent PASS"));
                Ok((Verdict::Pass, report))
            }
            Some(1) => {
                for line in report.lines() {
                    self.msg(&format!("  {label}: {line}"));
                }
                self.msg(&format!("  {label}: inline locations STALE FAIL"));
                Ok((Verdict::Fail, report))
            }
            _ => {
                self.require_success(&ended, "pbfhogg check --locations")?;
                Err(Error::tool("unexpected location check status"))
            }
        }
    }

    /// Buffered construction of an unindexed comparison input.
    pub(crate) fn strip_indexdata(&mut self, input: &Path, output: &Path) -> Result<(), Error> {
        let ended = self.pbfhogg_discard(
            PbfArgv::new("degrade")
                .arg("--strip-indexdata")
                .input(input)
                .output(output),
        )?;
        self.require_success(&ended, "pbfhogg degrade --strip-indexdata")
    }

    /// Whether `pbf`'s header declares `Sort.Type_then_ID` in either feature
    /// list, read by the verifier's own header reader.
    pub(crate) fn has_sort_flag(&mut self, pbf: &Path) -> Result<bool, Error> {
        Ok(pbf_frames::header(pbf)?.declares_sorted())
    }

    pub(crate) fn compare_sort_feature(
        &mut self,
        ours: &Path,
        theirs: &Path,
    ) -> Result<Verdict, Error> {
        let a = self.has_sort_flag(ours)?;
        let b = self.has_sort_flag(theirs)?;
        self.msg(&format!("  sort feature: pbfhogg {a}, reference {b}"));
        Ok(Verdict::from_pass(!b || a))
    }

    /// Judge `ours`'s `LocationsOnWays` declaration against what its command
    /// documents for `input` (docs/cli/commands.md "Output header"), with
    /// `ended` the run that wrote it. The element comparison reads inline
    /// coordinates only when both compared files declare them
    /// ([`Policy::between`]), so a declaration lost without this would pass
    /// silently. A command may drop the declaration only where its table
    /// says so, and then warns; that warning is read from stderr, the CLI's
    /// rendering of the library's `header.way_metadata_loss`. An output
    /// never declares it when the input did not. Judged on the file pbfhogg
    /// wrote, before any osmium-sorted copy.
    pub(crate) fn judge_way_locations(
        &mut self,
        label: &str,
        input: &Path,
        ours: &Path,
        ended: &Ended,
        expect: WayLocations,
    ) -> Result<(), Error> {
        const LOW: &str = "LocationsOnWays";
        let had = pbf_frames::header(input)?.optional_feature(LOW);
        let has = pbf_frames::header(ours)?.optional_feature(LOW);
        let warned = warns_locations_loss(&ended.stderr_text());
        let (ok, why) = match (had, has, expect) {
            (false, true, _) => (false, "declared, but the input does not declare it"),
            (false, false, _) => (true, "not declared, as the input does not"),
            (true, true, WayLocations::Kept | WayLocations::KeptOrReported) => {
                (true, "kept as the input declares")
            }
            (true, true, WayLocations::Dropped) => (
                false,
                "declared, but this command re-encodes ways without them",
            ),
            (true, false, WayLocations::Kept) => (false, "dropped, but this run must keep it"),
            (true, false, WayLocations::Dropped | WayLocations::KeptOrReported) if warned => {
                (true, "dropped with the loss warning")
            }
            (true, false, _) => (false, "dropped without the loss warning"),
        };
        self.msg(&format!(
            "  {label}: {LOW} {why} {}",
            if ok { "PASS" } else { "FAIL" }
        ));
        self.record(
            &format!("{label} output declares {LOW} as its command documents"),
            Verdict::from_pass(ok),
        );
        Ok(())
    }

    /// Run a declared reference run, its standard output in the file `sink`.
    /// A tool killed by
    /// SIGKILL, which the verifier never sends one while it waits, is an
    /// environment outcome: commonly the kernel OOM killer, and it says
    /// nothing about pbfhogg. The run's inputs are revalidated before its
    /// outcome is used.
    pub(crate) fn reference(&mut self, run: &ToolRun, sink: &Path) -> Result<Ended, Error> {
        let label = run.label();
        let started = Instant::now();
        let (ended, cached) =
            self.cache
                .execute(&mut self.life, &mut self.run, &self.tools, run, sink)?;
        if cached {
            self.msg(&format!("  {label}: cached reference"));
            self.step(&format!("{label} cached"), Duration::ZERO);
        } else {
            self.step(&label, started.elapsed());
        }
        if ended.signal() == Some(libc::SIGKILL) {
            return Err(Error::Environment(format!(
                "{label} (reference run {}) was killed by SIGKILL, which the verifier did not \
                 send (commonly the kernel OOM killer); the comparison could not be made",
                run.id
            )));
        }
        for input in run.inputs() {
            self.run.watch(input)?;
        }
        self.run.revalidate()?;
        if !ended
            .code()
            .is_some_and(|code| run.completes(code, &ended.stderr))
        {
            return Err(Error::Failed(format!(
                "{label} (reference run {}) failed: {}: {}",
                run.id,
                ended.status,
                ended.stderr_text().trim()
            )));
        }
        Ok(ended)
    }

    /// `Err` unless a pbfhogg launch exited 0.
    pub(crate) fn require_success(&self, ended: &Ended, program: &str) -> Result<(), Error> {
        if ended.status.success() {
            return Ok(());
        }
        Err(Error::Failed(format!(
            "{program} failed: {}: {}",
            ended.status,
            ended.stderr_text().trim()
        )))
    }

    /// Require an explicitly accepted answer, including a diff's exit 1.
    pub(crate) fn require_codes(
        &self,
        ended: &Ended,
        program: &str,
        codes: &[i32],
    ) -> Result<(), Error> {
        if ended.code().is_some_and(|code| codes.contains(&code)) {
            return Ok(());
        }
        Err(Error::Failed(format!(
            "{program} failed: {}: {}",
            ended.status,
            ended.stderr_text().trim()
        )))
    }

    /// osmium's reading of `pbf` (`osmium fileinfo -e`), its report beside
    /// it; counts and order go into the detail.
    pub(crate) fn fileinfo(&mut self, pbf: &Path) -> Result<FileInfo, Error> {
        // A file read twice in one check gets a fresh reading each time.
        let report = pbf.with_extension("fileinfo.json");
        match std::fs::remove_file(&report) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        verify_fileinfo::fileinfo(self, pbf, &report)
    }

    /// Print osmium's counts and order of `pbf` for context. A failed read
    /// is printed, not propagated (informational); an interrupt still is.
    pub(crate) fn print_fileinfo(&mut self, label: &str, pbf: &Path) -> Result<(), Error> {
        match self.fileinfo(pbf) {
            Ok(_) => {}
            Err(e @ (Error::Interrupted | Error::Unsettled(_))) => return Err(e),
            Err(e) => self.msg(&format!("osmium fileinfo failed for {label}: {e}")),
        }
        Ok(())
    }

    /// Compare the elements of `a` and `b` as osmium decodes them, under
    /// `policy` ([`opl_compare`]), the differing records in `listing`. Pass
    /// only when every object is on both sides with identical records. Both
    /// sides must be in strict kind and ID order, else the comparison could
    /// not be made (an error).
    pub(crate) fn same_elements(
        &mut self,
        a: &Path,
        b: &Path,
        listing: &Path,
        policy: Policy,
    ) -> Result<Verdict, Error> {
        let merged = opl_compare::compare(self, a, b, listing, policy, |_| {})?;
        let verdict = Verdict::from_pass(merged.identical());
        if verdict == Verdict::Fail {
            self.replay_listing(listing)?;
        }
        Ok(verdict)
    }

    pub(crate) fn replay_listing(&mut self, listing: &Path) -> Result<(), Error> {
        let (lines, more) = stream::preview(
            listing,
            tunables::PREVIEW_LINES,
            tunables::PREVIEW_LINE_BYTES,
        )?;
        for line in &lines {
            self.msg(line);
        }
        let shown = if more {
            "a bounded head"
        } else {
            "lines cut to a bound"
        };
        self.msg(&format!(
            "  (listing above is {shown}; all of it is in {})",
            listing.display()
        ));
        Ok(())
    }

    /// Whether `pbf`'s objects strictly ascend by kind and then ID as osmium
    /// decodes them ([`opl_compare::order`]: no ID repeated), never the
    /// header claim, which can be blind to intra-blob disorder.
    pub(crate) fn check_sorted(&mut self, label: &str, pbf: &Path) -> Result<Verdict, Error> {
        Ok(self.check_sorted_counted(label, pbf)?.0)
    }

    /// The same order verdict and osmium's counts from the same reading.
    pub(crate) fn check_sorted_counted(
        &mut self,
        label: &str,
        pbf: &Path,
    ) -> Result<(Verdict, crate::counts::Counts), Error> {
        let (ordered, [nodes, ways, relations]) =
            opl_compare::order(self, pbf, &pbf.with_extension("order.opl"))?;
        let counts = crate::counts::Counts {
            nodes,
            ways,
            relations,
        };
        self.msg(&format!(
            "  {label}: {nodes} nodes, {ways} ways, {relations} relations"
        ));
        if ordered {
            self.msg(&format!("  {label}: in strict order (osmium OPL) PASS"));
            return Ok((Verdict::Pass, counts));
        }
        let hint = if pbf_frames::header(pbf)?.declares_sorted() {
            format!(
                " - header declares {} but element order disagrees",
                pbf_frames::SORT_TYPE_THEN_ID
            )
        } else {
            String::new()
        };
        self.msg(&format!(
            "  {label}: NOT in strict order (osmium OPL){hint} FAIL"
        ));
        Ok((Verdict::Fail, counts))
    }

    /// Run one check and print its result line. `Err` is only an interrupt
    /// or a verifier error, all of which stop a suite.
    pub(crate) fn run_check(
        &mut self,
        name: &str,
        check: impl FnOnce(&mut Self) -> Result<Option<String>, Error>,
    ) -> Result<CheckStatus, Error> {
        let start = Instant::now();
        self.failures.clear();
        self.steps.clear();
        self.check_dir = None;
        let result = check(self);
        let validation = self.run.revalidate();
        let result = match result {
            Err(e @ (Error::Interrupted | Error::Unsettled(_))) => Err(e),
            result => validation.and(result).and_then(|claim| {
                self.checkpoint()?;
                Ok(claim)
            }),
        };
        let ms = self.timing(start.elapsed());
        let failures = std::mem::take(&mut self.failures);
        let (status, line) = conclude(name, &ms, result, &failures);
        if matches!(status, Ok(CheckStatus::Passed)) {
            self.detail.discard();
        } else {
            self.detail.flush();
        }
        output::result(&line);
        status
    }

    /// `12.0s; osmium sort 8.1s, pbfhogg diff 2.0s (2 runs)`, slowest first.
    fn timing(&mut self, total: Duration) -> String {
        let mut steps = std::mem::take(&mut self.steps);
        steps.sort_by_key(|(_, took, _)| std::cmp::Reverse(*took));
        let rendered: Vec<String> = steps
            .iter()
            .map(|(label, took, runs)| {
                let runs = if *runs > 1 {
                    format!(" ({runs} runs)")
                } else {
                    String::new()
                };
                if label.ends_with(" cached") {
                    format!("{label}{runs}")
                } else {
                    format!("{label} {}{runs}", output::duration(*took))
                }
            })
            .collect();
        if rendered.is_empty() {
            output::duration(total)
        } else {
            format!("{}; {}", output::duration(total), rendered.join(", "))
        }
    }
}

/// A check's status and result line from what its closure returned and the
/// failures recorded while it ran. A recorded failure is the verdict even
/// when the closure then returned `Ok` or an environment error.
fn conclude(
    name: &str,
    ms: &str,
    result: Result<Option<String>, Error>,
    failures: &[String],
) -> (Result<CheckStatus, Error>, String) {
    let summary = || {
        format!(
            "{} comparison(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    };
    match result {
        Ok(claim) if failures.is_empty() => (
            Ok(CheckStatus::Passed),
            match claim {
                Some(claim) => format!("{name}: PASS ({ms}): {claim}"),
                None => format!("{name}: PASS ({ms})"),
            },
        ),
        Ok(_) => (
            Ok(CheckStatus::Failed),
            format!("{name}: FAIL ({ms}): {}", summary()),
        ),
        Err(e @ Error::Interrupted) => {
            let line = format!("{name}: INTERRUPTED ({ms}): {e}");
            (Err(e), line)
        }
        Err(e @ (Error::Tool(_) | Error::Unsettled(_))) => {
            let recorded = if failures.is_empty() {
                String::new()
            } else {
                format!("{}; then ", summary())
            };
            let line = format!("{name}: ERROR ({ms}): {recorded}{e}");
            (Err(e), line)
        }
        Err(e @ Error::Environment(_)) if failures.is_empty() => (
            Ok(CheckStatus::Environment),
            format!("{name}: ENVIRONMENT ({ms}): {e}"),
        ),
        Err(e) => {
            let recorded = if failures.is_empty() {
                String::new()
            } else {
                format!("{}; then ", summary())
            };
            (
                Ok(CheckStatus::Failed),
                format!("{name}: FAIL ({ms}): {recorded}{e}"),
            )
        }
    }
}

/// Where a diff listing for a comparison involving `output` goes: beside it.
pub(crate) fn listing_beside(output: &Path) -> PathBuf {
    output.with_extension("diff.txt")
}

#[cfg(test)]
mod tests {
    use super::{CheckStatus, Error, conclude};

    #[test]
    fn a_recorded_failure_outlives_ok_and_environment() {
        let failed = vec!["ways-only counts".to_owned()];
        let (status, line) = conclude("x", "1ms", Ok(None), &failed);
        assert!(matches!(status, Ok(CheckStatus::Failed)));
        assert!(line.contains("ways-only counts"), "{line}");
        let env = || Err(Error::Environment("osmium killed".into()));
        let (status, line) = conclude("x", "1ms", env(), &failed);
        assert!(matches!(status, Ok(CheckStatus::Failed)));
        assert!(line.contains("then environment"), "{line}");
        let (status, _) = conclude("x", "1ms", env(), &[]);
        assert!(matches!(status, Ok(CheckStatus::Environment)));
        let (status, line) = conclude("x", "1ms", Err(Error::tool("cannot write")), &failed);
        assert!(matches!(status, Err(Error::Tool(_))));
        assert!(
            line.contains("ways-only counts; then cannot write"),
            "{line}"
        );
    }

    #[test]
    fn claims_ride_passes_and_interrupts_pass_through() {
        let (status, line) = conclude("extract", "5ms", Ok(Some("order only".into())), &[]);
        assert!(matches!(status, Ok(CheckStatus::Passed)));
        assert_eq!(line, "extract: PASS (5ms): order only");
        let (status, _) = conclude("x", "1ms", Err(Error::Interrupted), &[]);
        assert!(matches!(status, Err(Error::Interrupted)));
        let (status, _) = conclude("x", "1ms", Err(Error::Unsettled("tree".into())), &[]);
        assert!(matches!(status, Err(Error::Unsettled(_))));
        let (status, _) = conclude("x", "1ms", Err(Error::tool("boom")), &[]);
        assert!(matches!(status, Err(Error::Tool(_))));
    }

    #[test]
    fn the_loss_warning_names_locations_on_ways_exactly() {
        let line = |list: &str| {
            format!(
                "Warning: the input declares {list}, which the output does not declare: this \
                 command does not keep it on every way it writes, or cannot vouch for it in \
                 the output"
            )
        };
        assert!(super::warns_locations_loss(&line("LocationsOnWays")));
        assert!(super::warns_locations_loss(&format!(
            "progress\n{}\n",
            line("LocationsOnWays, pbfhogg.WayMembers-v1")
        )));
        assert!(!super::warns_locations_loss(&line("pbfhogg.WayMembers-v1")));
        assert!(!super::warns_locations_loss(&line("LocationsOnWaysX")));
        assert!(!super::warns_locations_loss(""));
    }
}
