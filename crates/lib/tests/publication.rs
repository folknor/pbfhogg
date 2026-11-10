//! The publication log (Decision B of the library harness design, ADR-0017,
//! ADR-0019): explicit binding, one decision per unit, ordinals in execution
//! order, invocation-wide cancel, and effects observed from the namespace
//! transitions each publication made.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use pbfhogg::host::Invocation;
use pbfhogg::{BoundOutput, CancelOutcome, Decision, Effect, Publication};

/// Bound on every park, under the 20 s check watchdog.
const PARK: Duration = Duration::from_secs(10);

fn rows(invocation: &Invocation) -> Vec<Publication> {
    let mut rows = Vec::new();
    invocation.for_each_publication(|row| rows.push(row.clone()));
    rows
}

/// `(ordinal, decision, effects)` of every row.
fn summary(invocation: &Invocation) -> Vec<(u64, Decision, Vec<Effect>)> {
    rows(invocation)
        .iter()
        .map(|row| {
            assert!(row.finished(), "{row:?}");
            (
                row.ordinal(),
                row.decision(),
                row.members()
                    .iter()
                    .map(pbfhogg::PublishedMember::effect)
                    .collect(),
            )
        })
        .collect()
}

fn stage(out: &BoundOutput, bytes: &[u8]) {
    let mut writer = out.writer().expect("writer");
    std::io::Write::write_all(&mut writer, bytes).expect("write");
    writer.finish().expect("finish");
}

fn staged(path: &Path, invocation: &Invocation) -> BoundOutput {
    let out = crate::common::out(path).bind(invocation).expect("open");
    stage(&out, b"new");
    out
}

/// Ordinals follow decisions, not binding: two outputs bound A then B and
/// published B then A are rows 1 (B) and 2 (A).
#[test]
fn reversed_creation_and_publication_order() {
    let dir = crate::common::test_scratch();
    let invocation = Invocation::new();
    let a = staged(&dir.path().join("a"), &invocation);
    let b = staged(&dir.path().join("b"), &invocation);
    b.publish().expect("b");
    a.publish().expect("a");
    let rows = rows(&invocation);
    let dests: Vec<PathBuf> = rows
        .iter()
        .map(|row| row.members()[0].destination().to_path_buf())
        .collect();
    assert_eq!(dests, [dir.path().join("b"), dir.path().join("a")]);
    assert_eq!(
        summary(&invocation),
        [
            (1, Decision::Authorized, vec![Effect::Installed]),
            (2, Decision::Authorized, vec![Effect::Installed]),
        ]
    );
}

/// A publication parked in its `before_publish` hook holds the invocation's
/// execution lock: a second publication waits undecided, and is row 2.
#[test]
fn ordinal_order_is_execution_order_with_a_hook_parked() {
    let dir = crate::common::test_scratch();
    let invocation = Invocation::new();
    let (parked_tx, parked_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = std::sync::Mutex::new(release_rx);
    let a = staged(&dir.path().join("a"), &invocation).before_publish(move || {
        parked_tx.send(()).expect("parked");
        release_rx
            .lock()
            .expect("release")
            .recv_timeout(PARK)
            .expect("released");
    });
    let b = staged(&dir.path().join("b"), &invocation);
    std::thread::scope(|scope| {
        let first = scope.spawn(move || a.publish());
        parked_rx.recv_timeout(PARK).expect("a parked in its hook");
        let second = scope.spawn(move || b.publish());
        std::thread::sleep(Duration::from_millis(50));
        let seen = rows(&invocation);
        assert_eq!(seen.len(), 1, "b waits undecided: {seen:?}");
        assert!(!seen[0].finished());
        assert!(!second.is_finished());
        release_tx.send(()).expect("release");
        first.join().expect("a").expect("a published");
        second.join().expect("b").expect("b published");
    });
    let rows = rows(&invocation);
    assert_eq!(rows[0].members()[0].destination(), dir.path().join("a"));
    assert_eq!(rows[1].members()[0].destination(), dir.path().join("b"));
    assert_eq!(
        summary(&invocation),
        [
            (1, Decision::Authorized, vec![Effect::Installed]),
            (2, Decision::Authorized, vec![Effect::Installed]),
        ]
    );
}

/// A set is one publication: one row, one member each.
#[test]
fn one_row_per_set() {
    let dir = crate::common::test_scratch();
    let invocation = Invocation::new();
    let paths: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| dir.path().join(n)).collect();
    let set = pbfhogg::OutputSet::files(
        &paths,
        pbfhogg::Overwrite::Refuse,
        &pbfhogg::host::Diagnostics::discard(),
    )
    .expect("set")
    .bind(&invocation)
    .expect("open");
    for member in set.iter() {
        stage(member, b"new");
    }
    set.publish().expect("published");
    assert_eq!(
        summary(&invocation),
        [(1, Decision::Authorized, vec![Effect::Installed; 3])]
    );
}

/// An empty set covers no destination, so it is refused at construction.
#[test]
fn an_empty_set_is_refused() {
    let err = pbfhogg::OutputSet::files(
        Vec::<PathBuf>::new(),
        pbfhogg::Overwrite::Refuse,
        &pbfhogg::host::Diagnostics::discard(),
    )
    .expect_err("empty");
    assert!(
        matches!(err.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{err}"
    );
}

/// A command bound its output at entry, so failing before it opened it
/// still decides it: `refused_error`, nothing done.
#[test]
fn a_command_failing_before_it_opens_its_output_records_refused_error() {
    let dir = crate::common::test_scratch();
    let host = crate::common::test_host();
    let missing = dir.path().join("missing.osm.pbf");
    let result = pbfhogg::cat::cat(
        &[missing.as_path()],
        crate::common::out(dir.path().join("out.osm.pbf")),
        None,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        &host,
    );
    assert!(result.is_err(), "no input");
    assert_eq!(
        summary(&host.invocation),
        [(1, Decision::RefusedError, vec![Effect::None])]
    );
}

/// Dropping one set member's writer is session state: the set stays
/// pending until its owner decides it.
#[test]
fn dropping_one_set_member_leaves_the_set_pending() {
    let dir = crate::common::test_scratch();
    let invocation = Invocation::new();
    let paths: Vec<PathBuf> = ["a", "b"].iter().map(|n| dir.path().join(n)).collect();
    let set = pbfhogg::OutputSet::files(
        &paths,
        pbfhogg::Overwrite::Refuse,
        &pbfhogg::host::Diagnostics::discard(),
    )
    .expect("set")
    .bind(&invocation)
    .expect("open");
    let writer = set.get(0).expect("member").writer().expect("writer");
    drop(writer);
    assert!(rows(&invocation).is_empty());
    drop(set);
    assert_eq!(
        summary(&invocation),
        [(1, Decision::RefusedError, vec![Effect::None; 2])]
    );
}

/// Cancel decides every pending unit, in bind order, and every later
/// decision refuses.
#[test]
fn cancel_decides_pending_units_in_bind_order() {
    let dir = crate::common::test_scratch();
    assert_eq!(
        Invocation::new().cancel(),
        CancelOutcome::NoPublicationPending
    );
    let invocation = Invocation::new();
    let a = staged(&dir.path().join("a"), &invocation);
    let b = staged(&dir.path().join("b"), &invocation);
    assert_eq!(
        invocation.cancel(),
        CancelOutcome::BeforeDecision { refused: 1 }
    );
    assert!(b.publish().expect_err("b").is_cancelled());
    assert!(a.publish().expect_err("a").is_cancelled());
    // Bound after the cancel (unwritten: its writer would refuse too).
    let late = crate::common::out(dir.path().join("c"))
        .bind(&invocation)
        .expect("open");
    assert!(late.publish().expect_err("late").is_cancelled());
    let rows = rows(&invocation);
    assert_eq!(rows[0].members()[0].destination(), dir.path().join("a"));
    assert_eq!(rows[1].members()[0].destination(), dir.path().join("b"));
    assert_eq!(
        summary(&invocation),
        [
            (1, Decision::RefusedCancelled, vec![Effect::None]),
            (2, Decision::RefusedCancelled, vec![Effect::None]),
            (3, Decision::RefusedCancelled, vec![Effect::None]),
        ]
    );
    assert!(!dir.path().join("a").exists());
}

/// A `before_publish` hook publishing under the same invocation is refused
/// as re-entry, never waited on; a unit a cancel decided first keeps its
/// cancellation.
#[test]
fn publication_cannot_be_reentered_from_a_hook() {
    let dir = crate::common::test_scratch();
    let invocation = Invocation::new();
    let inner = staged(&dir.path().join("inner"), &invocation);
    let cancelled = staged(&dir.path().join("cancelled"), &invocation);
    let (tx, rx) = mpsc::channel();
    let token = invocation.clone();
    let outer = staged(&dir.path().join("outer"), &invocation).before_publish(move || {
        let reentered = inner.publish().expect_err("re-entry");
        let outcome = token.cancel();
        let after_cancel = cancelled.publish().expect_err("cancelled");
        tx.send((reentered, outcome, after_cancel)).expect("send");
    });
    outer.publish().expect("outer published");
    let (reentered, outcome, after_cancel) = rx.recv().expect("hook ran");
    assert!(
        matches!(reentered.kind(), pbfhogg::ErrorKind::InvalidArgument { .. }),
        "{reentered}"
    );
    assert_eq!(outcome, CancelOutcome::BeforeDecision { refused: 3 });
    assert!(after_cancel.is_cancelled(), "{after_cancel}");
    assert_eq!(
        summary(&invocation),
        [
            (1, Decision::Authorized, vec![Effect::Installed]),
            (2, Decision::RefusedError, vec![Effect::None]),
            (3, Decision::RefusedCancelled, vec![Effect::None]),
        ]
    );
}

#[cfg(feature = "test-hooks")]
mod faults {
    use super::*;
    use pbfhogg::output::PublishFault;
    use pbfhogg::test_hooks::PublishStep;

    /// The cancel and the decision race on the log, parked from both
    /// sides. Parked before the decision (at its checkpoint), a cancel
    /// decides the unit first and the publisher finds it refused. Parked
    /// after it (in the hook), the cancel finds it authorized, and decides
    /// only a unit still pending.
    #[test]
    fn cancel_parked_against_a_decision_from_both_sides() {
        let dir = crate::common::test_scratch();

        // The cancel first.
        let invocation = Invocation::new();
        let (parked_tx, parked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        // Staged first: the writer passes checkpoints too.
        let out = staged(&dir.path().join("first"), &invocation);
        invocation.on_check(move |_, _| {
            parked_tx.send(()).expect("parked");
            release_rx
                .lock()
                .expect("release")
                .recv_timeout(PARK)
                .expect("released");
        });
        std::thread::scope(|scope| {
            let publisher = scope.spawn(move || out.publish());
            parked_rx
                .recv_timeout(PARK)
                .expect("parked at the decision");
            assert_eq!(
                invocation.cancel(),
                CancelOutcome::BeforeDecision { refused: 1 }
            );
            release_tx.send(()).expect("release");
            let err = publisher.join().expect("join").expect_err("refused");
            assert!(err.is_cancelled(), "{err}");
        });
        assert_eq!(
            summary(&invocation),
            [(1, Decision::RefusedCancelled, vec![Effect::None])]
        );
        assert!(!dir.path().join("first").exists());

        // The decision first.
        let invocation = Invocation::new();
        let (parked_tx, parked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let out = staged(&dir.path().join("second"), &invocation).before_publish(move || {
            parked_tx.send(()).expect("parked");
            release_rx
                .lock()
                .expect("release")
                .recv_timeout(PARK)
                .expect("released");
        });
        std::thread::scope(|scope| {
            let publisher = scope.spawn(move || out.publish());
            parked_rx
                .recv_timeout(PARK)
                .expect("parked after the decision");
            assert_eq!(
                invocation.cancel(),
                CancelOutcome::AfterDecision { authorized: 1 }
            );
            let pending = crate::common::out(dir.path().join("pending"))
                .bind(&invocation)
                .expect("open");
            assert_eq!(
                invocation.cancel(),
                CancelOutcome::BeforeDecision { refused: 2 }
            );
            release_tx.send(()).expect("release");
            publisher
                .join()
                .expect("join")
                .expect("a decided publication completes");
            assert!(pending.publish().expect_err("refused").is_cancelled());
        });
        assert_eq!(
            summary(&invocation),
            [
                (1, Decision::Authorized, vec![Effect::Installed]),
                (2, Decision::RefusedCancelled, vec![Effect::None]),
            ]
        );
    }

    /// One publication under `faults`, of `names` (a set when more than
    /// one), each destination holding `old` first when `existing`; returns
    /// its effects.
    fn effects(names: &[&str], existing: bool, faults: &[PublishFault]) -> Vec<Effect> {
        let dir = crate::common::test_scratch();
        let invocation = Invocation::new();
        for &fault in faults {
            invocation.faults().arm_publish_fault(fault);
        }
        let paths: Vec<PathBuf> = names.iter().map(|n| dir.path().join(n)).collect();
        if existing {
            for path in &paths {
                std::fs::write(path, b"old").expect("old");
            }
        }
        let overwrite = pbfhogg::Overwrite::Replace;
        let result = if let [path] = paths.as_slice() {
            let out = crate::common::out_replace(path)
                .bind(&invocation)
                .expect("open");
            stage(&out, b"new");
            out.publish()
        } else {
            let set = pbfhogg::OutputSet::files(
                &paths,
                overwrite,
                &pbfhogg::host::Diagnostics::discard(),
            )
            .expect("set")
            .bind(&invocation)
            .expect("open");
            for member in set.iter() {
                stage(member, b"new");
            }
            set.publish()
        };
        let summary = summary(&invocation);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].1, Decision::Authorized);
        // Every effect replays to what is on disk.
        for (path, effect) in paths.iter().zip(&summary[0].2) {
            let now = std::fs::read(path).ok();
            let expected = match effect {
                Effect::Installed => Some(b"new".to_vec()),
                Effect::Restored => Some(b"old".to_vec()),
                Effect::Displaced => None,
                Effect::None => existing.then(|| b"old".to_vec()),
            };
            assert_eq!(now, expected, "{} {effect:?} {result:?}", path.display());
        }
        summary[0].2.clone()
    }

    #[test]
    fn every_effect_through_publication_faults() {
        use Effect::{Displaced, Installed, None, Restored};
        // Success, fresh and replacing.
        assert_eq!(effects(&["a"], false, &[]), [Installed]);
        assert_eq!(effects(&["a"], true, &[]), [Installed]);
        // Before any namespace change.
        assert_eq!(
            effects(&["a"], true, &[PublishFault::BeforeInstall]),
            [None]
        );
        assert_eq!(
            effects(&["a", "b"], true, &[PublishFault::MoveAside]),
            [None, None]
        );
        // A new generation removed again: none.
        assert_eq!(
            effects(&["a"], false, &[PublishFault::AfterInstalled(0)]),
            [None]
        );
        // A kept unbacked replacement: installed (L0).
        assert_eq!(
            effects(&["a"], true, &[PublishFault::AfterInstalled(0)]),
            [Installed]
        );
        // Moved aside and put back.
        assert_eq!(
            effects(&["a", "b"], true, &[PublishFault::AfterBackup]),
            [Restored, None]
        );
        // Moved aside and not put back.
        assert_eq!(
            effects(
                &["a", "b"],
                true,
                &[PublishFault::AfterBackup, PublishFault::Restore]
            ),
            [Displaced, None]
        );
        // A rollback that cannot take the new generation out.
        assert_eq!(
            effects(
                &["a", "b"],
                true,
                &[PublishFault::AfterInstalled(0), PublishFault::Restore]
            ),
            [Installed, None]
        );
        // Unsynced and backup-removal failures after installation.
        assert_eq!(
            effects(&["a", "b"], true, &[PublishFault::DirSync]),
            [Installed, Installed]
        );
        assert_eq!(
            effects(&["a", "b"], true, &[PublishFault::BackupRemoval]),
            [Installed, Installed]
        );
    }

    /// A directory taken back out whose previous generation fails to come
    /// back is displaced; with nothing to put back, it is none.
    #[test]
    fn a_directory_taken_out_is_displaced_or_none() {
        for existing in [true, false] {
            let dir = crate::common::test_scratch();
            let dest = dir.path().join("index");
            if existing {
                std::fs::create_dir(&dest).expect("old");
                std::fs::write(dest.join("old"), b"old").expect("old");
            }
            let invocation = Invocation::new();
            invocation
                .faults()
                .arm_publish_fault(PublishFault::AfterInstalled(0));
            invocation
                .faults()
                .arm_publish_fault(PublishFault::RestoreRename);
            let out = pbfhogg::OutputDir::new(
                &dest,
                pbfhogg::Overwrite::Replace,
                None,
                &pbfhogg::host::Diagnostics::discard(),
            )
            .expect("dir")
            .bind(&invocation)
            .expect("open");
            let writer = out.writer().expect("writer");
            std::fs::write(writer.path().join("new"), b"new").expect("new");
            writer.finish();
            out.publish().expect_err("injected");
            let expected = if existing {
                Effect::Displaced
            } else {
                Effect::None
            };
            assert_eq!(
                summary(&invocation),
                [(1, Decision::Authorized, vec![expected])]
            );
            assert!(!dest.exists());
        }
    }

    /// A group whose middle member fails: every member gets its own result,
    /// the untouched one after it included.
    #[test]
    fn a_group_with_a_failing_middle_member() {
        assert_eq!(
            effects(&["a", "b", "c"], true, &[PublishFault::AfterInstalled(1)]),
            [Effect::Restored, Effect::Restored, Effect::None]
        );
        assert_eq!(
            effects(&["a", "b", "c"], false, &[PublishFault::AfterInstalled(1)]),
            [Effect::None, Effect::None, Effect::None]
        );
    }

    /// A step observer that panics unwinds through publication; the row is
    /// still finalized from the transitions made, and the invocation
    /// publishes again afterwards.
    #[test]
    fn an_unwinding_publication_finalizes_its_row() {
        let dir = crate::common::test_scratch();
        let invocation = Invocation::new();
        let armed = std::sync::atomic::AtomicBool::new(true);
        invocation.faults().on_publish_step(move |step| {
            if step == PublishStep::Installed(0)
                && armed.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("observer");
            }
        });
        let out = staged(&dir.path().join("a"), &invocation);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| out.publish()));
        assert!(unwound.is_err());
        let second = staged(&dir.path().join("b"), &invocation);
        second.publish().expect("publishes again");
        assert_eq!(
            summary(&invocation),
            [
                (1, Decision::Authorized, vec![Effect::Installed]),
                (2, Decision::Authorized, vec![Effect::Installed]),
            ]
        );
    }
}
