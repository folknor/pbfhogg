//! One pinned-input fault cell, run whole: the reference run, its exact
//! transport witness, and the replacement matrix, all owned here.
//!
//! A cell is one route of one command, in one read mode, with one input as
//! the replacement target. [`check`] arms the target's replacement on a
//! fresh invocation past its last open and runs the command's reference:
//! the opens must all verify, none fire, and their transports must equal the
//! cell's spec letter for letter (`D` for an `O_DIRECT` open, `B` for a
//! buffered one). A cell whose spec holds a `D` must also record an
//! `O_DIRECT` read open, and a direct cell whose spec holds none must record
//! no `O_DIRECT` read open at all. Then the matrix: the replacement armed
//! after each open in turn (the last too unless the cell says otherwise),
//! in both reopen strategies, each run on a fresh invocation. The driver
//! judges every run itself, from that run's own report: the uniform rule is
//! that a pinned reopen, or a replacement after the last open, leaves the
//! run unaffected, and a path reopen before the last open refuses the next
//! open with `InputReplaced`. Each matrix run holds the same direct-open
//! count rule as the reference (a refused run, which may stop before its
//! first direct open, only the half that forbids one). The command's
//! Last, the caller-pin arm in both reopen strategies: the target replaced
//! after the step's pin is taken and before the call (`Step::after` 0),
//! where the stable-name pin must read the pinned file with the cell's
//! exact transports and the path pin must refuse its first open. That arm
//! is what catches a run that pins the path afresh on the step's own host.
//! The command's
//! callbacks prepare the fixtures, run the command and check its answer;
//! none of them decides whether a later phase runs, because the driver
//! calls each in turn.
//!
//! This is what makes a backend cell unable to pass unexercised: the one
//! early exit on Unix is the reference's typed `direct-io` setup refusal,
//! accepted only on a cell whose spec owes a direct open and only through
//! `common::direct_io_unavailable` (which fails the run where the gate
//! requires direct I/O), and it ends that cell alone, never the test. Off
//! Unix the matrix, which needs file identity verification, is skipped
//! after the reference, announced once per cell. Every other path through
//! a cell ends in a checked witness. A loop of
//! cells that returned early from one of them would need a return the
//! callbacks cannot express: they return values, not control.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pbfhogg::host::Host;
use pbfhogg::read::input::PinnedInput;
use pbfhogg::test_hooks::{InputReplacement, Transport};

use crate::reopen_cell::Scenario;

/// The read modes every cell runs in: buffered always, and direct where
/// the build has `linux-direct-io`.
pub fn modes() -> &'static [bool] {
    if cfg!(feature = "linux-direct-io") {
        &[false, true]
    } else {
        &[false]
    }
}

/// The bytes of the output at `path`, which is then removed, so the next
/// run writes into an absent destination.
pub fn take(path: &Path) -> Vec<u8> {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("read output {}: {e}", path.display()));
    std::fs::remove_file(path).unwrap_or_else(|e| panic!("remove output {}: {e}", path.display()));
    bytes
}

/// One cell's declaration.
pub struct Cell<'a> {
    what: String,
    #[cfg_attr(
        not(feature = "linux-direct-io"),
        expect(dead_code, reason = "only a direct cell reads it")
    )]
    direct: bool,
    target: &'a Path,
    replacement: &'a Path,
    opens: u64,
    spec: String,
    arm_last: bool,
    racing: Option<u64>,
    scratch: Option<&'a Path>,
    host: Box<dyn Fn() -> Host + 'a>,
}

impl<'a> Cell<'a> {
    /// A cell replacing `target` with the file at `replacement` (written by
    /// the matrix's `prepare`), whose run opens `target` `opens` times
    /// through its pin. `direct_spec` is the transport of each open on a
    /// direct run; a buffered run opens every time buffered.
    pub fn new(
        what: impl Into<String>,
        direct: bool,
        target: &'a Path,
        replacement: &'a Path,
        opens: u64,
        direct_spec: &str,
    ) -> Self {
        let what = format!("{}, direct {direct}", what.into());
        let spec = if direct {
            direct_spec.to_string()
        } else {
            "B".repeat(usize::try_from(opens).expect("open count"))
        };
        assert_eq!(
            spec.len() as u64,
            opens,
            "{what}: the transport spec {spec:?} must name every open"
        );
        assert!(
            spec.chars().all(|c| c == 'D' || c == 'B'),
            "{what}: transport spec {spec:?}"
        );
        Self {
            what,
            direct,
            target,
            replacement,
            opens,
            spec,
            arm_last: true,
            racing: None,
            scratch: None,
            host: Box::new(crate::common::test_host),
        }
    }

    /// Leave the last open out of the matrix: the command reads or writes
    /// past its last open in a way the uniform rule does not describe.
    pub fn without_last_arm(mut self) -> Self {
        self.arm_last = false;
        self
    }

    /// After open `after`, two concurrent opens follow the replacement and
    /// a path reopen refuses both.
    pub fn racing(mut self, after: Option<u64>) -> Self {
        self.racing = after;
        self
    }

    /// Hold every refused run to leaving `dir` as it found it, but for the
    /// replacement renamed over the target.
    pub fn scratch(mut self, dir: &'a Path) -> Self {
        self.scratch = Some(dir);
        self
    }

    /// Make each run's host with `host` (fresh per run) instead of
    /// `common::test_host`.
    pub fn host(mut self, host: impl Fn() -> Host + 'a) -> Self {
        self.host = Box::new(host);
        self
    }

    /// The cell's name, for the command's own messages.
    pub fn what(&self) -> &str {
        &self.what
    }

    #[cfg_attr(
        not(feature = "linux-direct-io"),
        expect(dead_code, reason = "only a direct cell asks")
    )]
    fn owes_direct(&self) -> bool {
        self.spec.contains('D')
    }
}

/// One run of the matrix, as the command's callbacks see it.
pub struct Step<'a> {
    pub after: u64,
    pub scenario: Scenario,
    /// The pin the run must be handed.
    pub pin: &'a PinnedInput,
    pub host: &'a Host,
    /// The cell and the arm, for messages.
    pub what: String,
}

/// What the driver found a matrix run did.
pub enum Judged<O> {
    /// The run succeeded and opened the target as the reference did.
    Unaffected(O),
    /// The run refused the replaced input. The driver checked the error's
    /// kind and path and, for a cell with a scratch directory, that the
    /// directory is as it was.
    Refused,
}

/// How a cell ended.
pub enum Ran {
    /// The reference and every matrix run were witnessed.
    Witnessed,
    /// The host refused `O_DIRECT` setup on a cell that owes a direct open.
    Refused,
    /// The reference was witnessed; the matrix needs Unix file identity
    /// verification, which this platform lacks.
    ReferenceOnly,
}

/// Run `cell`. `reference` runs the command unreplaced (on paths, or
/// through a pin it takes itself where the entry takes none); `expected`
/// checks its answer and makes what the matrix compares against;
/// `prepare` rewrites the fixtures (the target, the replacement, any
/// existing destination) before each matrix run; `run` runs the command
/// through the step's pin; `validate` checks a run's answer, or its
/// publication when refused.
pub fn check<T, E, P, O>(
    cell: &Cell<'_>,
    reference: impl FnOnce(&Host) -> pbfhogg::Result<T>,
    expected: impl FnOnce(T) -> E,
    mut prepare: impl FnMut(u64, Scenario) -> P,
    mut run: impl FnMut(&P, &Step<'_>) -> pbfhogg::Result<O>,
    mut validate: impl FnMut(&E, &P, &Step<'_>, Judged<O>),
) -> Ran {
    let what = &cell.what;
    let unused = cell
        .target
        .with_file_name("never-renamed-over-the-input.osm.pbf");
    let host = (cell.host)();
    host.invocation
        .faults()
        .arm_input_replacement(cell.target, &unused, cell.opens + 1);
    let answer = match reference(&host) {
        Ok(answer) => answer,
        Err(e) if refusal_skips(cell, &e) => {
            crate::common::skip(format!("{what}: {e}"));
            return Ran::Refused;
        }
        Err(e) => panic!("{what}: unreplaced run: {}", e.report()),
    };
    let counted = report(&host, cell, &unused, cell.opens + 1);
    assert!(!counted.fired, "{what}: {counted:?}");
    assert_eq!(counted.attempts, cell.opens, "{what}: {counted:?}");
    assert_eq!(counted.verified, cell.opens, "{what}: {counted:?}");
    assert_eq!(counted.refused, 0, "{what}: {counted:?}");
    assert_eq!(counted.attempts_after_fired, 0, "{what}: {counted:?}");
    assert_eq!(
        letters(&counted.transports),
        cell.spec,
        "{what}: pinned-input transports"
    );
    witness_direct_count(cell, &host, true);
    let expected = expected(answer);
    if cfg!(not(unix)) {
        crate::common::skip(format!(
            "{what}: the replacement matrix: Unix file identity verification is unavailable"
        ));
        return Ran::ReferenceOnly;
    }

    let last = if cell.arm_last {
        cell.opens
    } else {
        cell.opens - 1
    };
    for after in 1..=last {
        for scenario in [Scenario::Pinned, Scenario::Path] {
            let prepared = prepare(after, scenario);
            let host = (cell.host)();
            // The step's outcome, carried out of `Scenario::run` so that its
            // scratch link is removed before a failure unwinds on.
            let mut outcome = None;
            scenario.run(cell.target, &host, |pin, scenario| {
                outcome = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || {
                        host.invocation.faults().arm_input_replacement(
                            cell.target,
                            cell.replacement,
                            after,
                        );
                        let before = cell.scratch.map(crate::common::snapshot_dir);
                        let step = Step {
                            after,
                            scenario,
                            pin,
                            host: &host,
                            what: format!(
                                "{what}, replaced after open {after}, scenario {scenario:?}"
                            ),
                        };
                        let result = run(&prepared, &step);
                        let verdict = judge(cell, &step, result, before.as_ref());
                        validate(&expected, &prepared, &step, verdict);
                    },
                )));
            });
            match outcome {
                Some(Ok(())) => {}
                Some(Err(failure)) => std::panic::resume_unwind(failure),
                // On Unix (the only platform that reaches the matrix)
                // `Scenario::run` always runs the step.
                None => panic!(
                    "{what}: the run replaced after open {after}, scenario {scenario:?}, was not judged"
                ),
            }
        }
    }
    for scenario in [Scenario::Pinned, Scenario::Path] {
        caller_pin_arm(
            cell,
            scenario,
            &expected,
            &mut prepare,
            &mut run,
            &mut validate,
        );
    }
    Ran::Witnessed
}

/// The caller-pin arm: the target is replaced after the caller pinned it
/// and before the call (`Step::after` 0). The stable-name pin must read the
/// file the caller pinned, every open through it, with the cell's exact
/// transports; the path pin must refuse its first open. This is what catches
/// a run that pins the path afresh on the step's own host, which the matrix
/// cannot see on a route with one open.
fn caller_pin_arm<E, P, O>(
    cell: &Cell<'_>,
    scenario: Scenario,
    expected: &E,
    prepare: &mut impl FnMut(u64, Scenario) -> P,
    run: &mut impl FnMut(&P, &Step<'_>) -> pbfhogg::Result<O>,
    validate: &mut impl FnMut(&E, &P, &Step<'_>, Judged<O>),
) {
    let what = &cell.what;
    let prepared = prepare(0, scenario);
    let host = (cell.host)();
    let unused = cell
        .target
        .with_file_name("never-renamed-over-the-input.osm.pbf");
    let mut outcome = None;
    scenario.run(cell.target, &host, |pin, scenario| {
        outcome = Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || {
                std::fs::rename(cell.replacement, cell.target)
                    .unwrap_or_else(|e| panic!("{what}: replace the input before the call: {e}"));
                host.invocation.faults().arm_input_replacement(
                    cell.target,
                    &unused,
                    cell.opens + 1,
                );
                let before = cell.scratch.map(crate::common::snapshot_dir);
                let step = Step {
                    after: 0,
                    scenario,
                    pin,
                    host: &host,
                    what: format!("{what}, replaced before the call, scenario {scenario:?}"),
                };
                let result = run(&prepared, &step);
                let report = report(&host, cell, &unused, cell.opens + 1);
                let what = &step.what;
                assert!(!report.fired, "{what}: {report:?}");
                let verdict = if scenario == Scenario::Pinned {
                    let out = match result {
                        Ok(out) => out,
                        Err(e) if refusal_skips(cell, &e) => {
                            crate::common::skip(format!("{what}: {e}"));
                            return;
                        }
                        Err(e) => panic!("{what}: the caller's pin must be read: {}", e.report()),
                    };
                    assert_eq!(report.attempts, cell.opens, "{what}: {report:?}");
                    assert_eq!(report.verified, cell.opens, "{what}: {report:?}");
                    assert_eq!(report.refused, 0, "{what}: {report:?}");
                    assert_eq!(
                        letters(&report.transports),
                        cell.spec,
                        "{what}: pinned-input transports"
                    );
                    witness_direct_count(cell, &host, true);
                    Judged::Unaffected(out)
                } else {
                    let err = result.err().unwrap_or_else(|| {
                        panic!(
                            "{what}: a path reopen of an input replaced before the call must refuse"
                        )
                    });
                    assert!(
                        matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                        "{what}: {err:?}"
                    );
                    assert_eq!(err.source_path(), Some(cell.target), "{what}: {err:?}");
                    assert_eq!(report.refused, 1, "{what}: {report:?}");
                    assert_eq!(report.attempts, 1, "{what}: {report:?}");
                    assert_eq!(report.verified, 0, "{what}: {report:?}");
                    witness_direct_count(cell, &host, false);
                    if let (Some(dir), Some(before)) = (cell.scratch, before.as_ref()) {
                        crate::common::assert_scratch_unchanged(
                            before,
                            &crate::common::snapshot_dir(dir),
                        );
                    }
                    Judged::Refused
                };
                validate(expected, &prepared, &step, verdict);
            },
        )));
    });
    match outcome {
        Some(Ok(())) => {}
        Some(Err(failure)) => std::panic::resume_unwind(failure),
        None => panic!("{what}: the caller-pin run, scenario {scenario:?}, was not judged"),
    }
}

/// Whether `err` ends the cell as a skip: only a typed `direct-io` setup
/// refusal, and only on a cell whose spec owes a direct open. A cell that
/// owes none must not reach `O_DIRECT` at all, so a refusal there is a
/// failure.
fn refusal_skips(cell: &Cell<'_>, err: &pbfhogg::Error) -> bool {
    #[cfg(feature = "linux-direct-io")]
    {
        cell.direct && cell.owes_direct() && crate::common::direct_io_unavailable(err)
    }
    #[cfg(not(feature = "linux-direct-io"))]
    {
        let _ = (cell, err);
        false
    }
}

/// The run's report, held to the arm the driver installed.
fn report(host: &Host, cell: &Cell<'_>, replacement: &Path, after: u64) -> InputReplacement {
    let report = host
        .invocation
        .faults()
        .input_replacement()
        .unwrap_or_else(|| panic!("{}: the replacement was armed", cell.what));
    assert_eq!(
        (
            report.target.as_path(),
            report.replacement.as_path(),
            report.after
        ),
        (cell.target, replacement, after),
        "{}: the report is of another arm",
        cell.what
    );
    report
}

/// The invocation's `O_DIRECT` read opens against the cell's spec: none on
/// a direct cell whose spec owes none, and, where `whole` (a run that made
/// every open), at least one on a cell whose spec owes one. A refused
/// matrix run may stop before its first direct open, so it asks only the
/// former.
fn witness_direct_count(cell: &Cell<'_>, host: &Host, whole: bool) {
    #[cfg(feature = "linux-direct-io")]
    if cell.direct {
        let opens = crate::common::direct_opens(&host.invocation, "read");
        if cell.owes_direct() {
            if !whole {
                return;
            }
            assert!(
                opens > 0,
                "{}: no O_DIRECT read open was recorded",
                cell.what
            );
        } else {
            assert_eq!(
                opens, 0,
                "{}: a route that owes no direct open opened O_DIRECT",
                cell.what
            );
        }
    }
    #[cfg(not(feature = "linux-direct-io"))]
    let _ = (cell, host, whole);
}

fn letters(transports: &[Transport]) -> String {
    transports
        .iter()
        .map(|t| match t {
            Transport::Direct => 'D',
            Transport::Buffered => 'B',
        })
        .collect()
}

fn judge<O>(
    cell: &Cell<'_>,
    step: &Step<'_>,
    result: pbfhogg::Result<O>,
    before: Option<&BTreeSet<PathBuf>>,
) -> Judged<O> {
    let what = &step.what;
    let report = report(step.host, cell, cell.replacement, step.after);
    assert!(
        report.fired,
        "{what}: the replacement must fire: {report:?}"
    );
    let opens = cell.opens;
    let after = step.after;
    if step.scenario == Scenario::Pinned || after == opens {
        let out = result.unwrap_or_else(|e| {
            panic!(
                "{what}: the run is unaffected by the replacement: {}",
                e.report()
            )
        });
        assert_eq!(report.attempts, opens, "{what}: {report:?}");
        assert_eq!(report.verified, opens, "{what}: {report:?}");
        assert_eq!(report.refused, 0, "{what}: {report:?}");
        assert_eq!(
            report.attempts_after_fired,
            opens - after,
            "{what}: {report:?}"
        );
        assert_eq!(
            letters(&report.transports),
            cell.spec,
            "{what}: pinned-input transports"
        );
        witness_direct_count(cell, step.host, true);
        return Judged::Unaffected(out);
    }
    let err = result
        .err()
        .unwrap_or_else(|| panic!("{what}: a path reopen of the replaced input must refuse"));
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
        "{what}: {err:?}"
    );
    assert_eq!(err.source_path(), Some(cell.target), "{what}: {err:?}");
    // The run stops at the first open after the replacement, except where
    // two opens race past it together.
    let refused = if cell.racing == Some(after) { 2 } else { 1 };
    assert_eq!(report.refused, refused, "{what}: {report:?}");
    assert_eq!(report.verified, after, "{what}: {report:?}");
    assert_eq!(report.attempts, after + refused, "{what}: {report:?}");
    assert_eq!(report.attempts_after_fired, refused, "{what}: {report:?}");
    assert_eq!(
        letters(&report.transports),
        cell.spec[..usize::try_from(after).expect("open count")],
        "{what}: the verified opens' transports"
    );
    witness_direct_count(cell, step.host, false);
    if let (Some(dir), Some(before)) = (cell.scratch, before) {
        // The replacement was renamed over the target, so it is the one
        // path the run itself did not account for.
        let mut after_run = crate::common::snapshot_dir(dir);
        after_run.insert(
            cell.replacement
                .strip_prefix(dir)
                .expect("the replacement lives in the scratch directory")
                .to_path_buf(),
        );
        crate::common::assert_scratch_unchanged(before, &after_run);
    }
    Judged::Refused
}

// The driver's own cells, on a toy command that opens its pin `n` times,
// buffered, and returns the bytes of the last open.

#[cfg(test)]
mod tests {
    use super::{Cell, Judged, Ran, Step, check};
    use crate::reopen_cell::Scenario;
    use std::io::Read as _;
    use std::path::Path;

    fn toy<'a>(
        source: impl Into<pbfhogg::read::input::InputSource<'a>> + Copy,
        n: u64,
    ) -> pbfhogg::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for _ in 0..n {
            let file = pbfhogg::read::input::InputFile::open(source)?;
            bytes.clear();
            (&**file.file())
                .read_to_end(&mut bytes)
                .map_err(pbfhogg::Error::from)?;
        }
        Ok(bytes)
    }

    fn write(dir: &Path) {
        std::fs::write(dir.join("in"), b"original").expect("write input");
        std::fs::write(dir.join("replacement"), b"replacement").expect("write replacement");
    }

    /// Run the toy cell in `dir`: `declared` opens claimed,
    /// `reference_opens` taken by the reference, and the matrix run `run`
    /// given the step.
    fn toy_cell(
        dir: &Path,
        declared: u64,
        reference_opens: u64,
        run: impl FnMut(&(), &Step<'_>) -> pbfhogg::Result<Vec<u8>>,
    ) -> Ran {
        write(dir);
        let input = dir.join("in");
        let replacement = dir.join("replacement");
        let cell = Cell::new("toy", false, &input, &replacement, declared, "").scratch(dir);
        check(
            &cell,
            |host| {
                let pin = pbfhogg::read::input::PinnedInput::pin(&input, host)?;
                toy(&pin, reference_opens)
            },
            |bytes| bytes,
            |_, _| write(dir),
            run,
            |expected, (), step, judged| {
                if let Judged::Unaffected(bytes) = judged {
                    assert_eq!(&bytes, expected, "{}", step.what);
                }
            },
        )
    }

    /// `cell`, run in a scratch directory that outlives it, fails with a
    /// panic naming `needle`; the directory is then removed as usual.
    fn fails(needle: &str, cell: impl FnOnce(&Path)) {
        let dir = crate::common::test_scratch();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cell(dir.path())));
        pbfhogg_test_scratch::unwound(result, needle);
    }

    #[cfg(unix)]
    #[test]
    fn a_cell_witnesses_its_reference_and_every_arm() {
        let dir = crate::common::test_scratch();
        let mut arms = Vec::new();
        let ran = toy_cell(dir.path(), 2, 2, |(), step| {
            arms.push((step.after, step.scenario));
            toy(step.pin, 2)
        });
        assert!(matches!(ran, Ran::Witnessed));
        assert_eq!(
            arms,
            [
                (1, Scenario::Pinned),
                (1, Scenario::Path),
                (2, Scenario::Pinned),
                (2, Scenario::Path),
                (0, Scenario::Pinned),
                (0, Scenario::Path)
            ]
        );
    }

    #[test]
    fn a_reference_that_opens_less_than_declared_fails() {
        fails("attempts", |dir| {
            toy_cell(dir, 2, 1, |(), step| toy(step.pin, 2));
        });
    }

    /// A matrix run that took a fresh pin of the path on another host,
    /// rather than the step's, is not the counted run the driver judges.
    #[cfg(unix)]
    #[test]
    fn a_run_that_ignores_the_steps_pin_fails() {
        fails("the replacement must fire", |dir| {
            toy_cell(dir, 2, 2, |(), step| {
                let fresh = pbfhogg::read::input::PinnedInput::pin(
                    step.pin.path(),
                    &crate::common::test_host(),
                )?;
                toy(&fresh, 2)
            });
        });
    }

    /// A one-open run that pins the path afresh on the step's own host
    /// passes the matrix (both pins name the original file until the
    /// replacement) and fails the caller-pin arm, where it reads the
    /// replacement.
    #[cfg(unix)]
    #[test]
    fn a_run_that_pins_the_path_afresh_fails() {
        fails("replaced before the call", |dir| {
            toy_cell(dir, 1, 1, |(), step| {
                let fresh = pbfhogg::read::input::PinnedInput::pin(step.pin.path(), step.host)?;
                toy(&fresh, 1)
            });
        });
    }

    /// A run that returns early, opening less than the route does, is
    /// judged from its own report and fails.
    #[cfg(unix)]
    #[test]
    fn a_run_that_returns_early_fails() {
        fails("the replacement must fire", |dir| {
            toy_cell(dir, 2, 2, |(), step| {
                if step.after == 2 {
                    return toy(step.pin, 1);
                }
                toy(step.pin, 2)
            });
        });
    }

    /// A direct cell whose route owes no direct open does not skip on a
    /// direct-io setup refusal: the route should never have reached one.
    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn a_direct_io_refusal_on_a_route_that_owes_no_direct_open_fails() {
        fails("unreplaced run", |dir| {
            write(dir);
            let input = dir.join("in");
            let replacement = dir.join("replacement");
            let cell = Cell::new("toy", true, &input, &replacement, 1, "B");
            check(
                &cell,
                |_| -> pbfhogg::Result<()> {
                    Err(pbfhogg::Error::platform_unsupported(
                        "direct-io",
                        std::io::Error::from_raw_os_error(22),
                    ))
                },
                |()| (),
                |_, _| (),
                |(), _| Ok(()),
                |(), (), _, _: Judged<()>| {},
            );
        });
    }
}
