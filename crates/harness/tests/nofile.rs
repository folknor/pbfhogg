//! Real descriptor probes run in a restricted, single-test child. The parent
//! never changes its limits, so sibling tests cannot affect these snapshots.

use pbfhogg::process::{FdAdjustment, FdCount, count_open_descriptors, current_nofile};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

#[path = "../src/process.rs"]
mod policy;

const CHILD: &str = "PBFHOGG_NOFILE_CONTRACT_CHILD";
// Fixture limits: enough for the loader and libtest, small enough to exhaust.
const SOFT: libc::rlim_t = 32;
const HARD: libc::rlim_t = 64;

fn limits() -> libc::rlimit {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: an initialized, exclusive output structure.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) },
        0
    );
    limit
}

fn lower_soft() {
    let limit = libc::rlimit {
        rlim_cur: SOFT,
        rlim_max: HARD,
    };
    // SAFETY: this runs only in the isolated contract child.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) },
        0
    );
}

/// Independent oracle: fcntl does not open the procfs enumeration descriptor.
fn counted(limit: libc::rlim_t) -> u64 {
    (0..i32::try_from(limit).expect("fixture limit fits an fd"))
        .filter(|fd| {
            // SAFETY: F_GETFD only queries the descriptor number.
            let result = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
            if result < 0 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
            }
            result >= 0
        })
        .count() as u64
}

fn check_count(limit: libc::rlim_t) {
    // The procfs iterator counts its own open directory as well.
    let expected = counted(limit) + 1;
    assert_eq!(count_open_descriptors(limit), FdCount::Counted(expected));
    let allowance = current_nofile();
    let probe = allowance.probe.expect("real probe");
    assert_eq!(probe.soft_limit, limit);
    assert_eq!(probe.hard_limit, Some(HARD));
    assert_eq!(probe.adjustment, None);
    assert_eq!(probe.open, FdCount::Counted(expected));
    assert_eq!(allowance.descriptors, limit - expected);
}

#[test]
fn restricted_child() {
    // CHILD's value, as a literal for the source-policy `environment` rule.
    if std::env::var_os("PBFHOGG_NOFILE_CONTRACT_CHILD").is_none() {
        return;
    }
    assert_eq!(limits().rlim_cur, SOFT);
    assert_eq!(limits().rlim_max, HARD);
    check_count(SOFT);
    let held = File::open("/dev/null").expect("one additional descriptor");
    check_count(SOFT);
    drop(held);
    check_count(SOFT);

    let raised = pbfhogg::process::raise_nofile_to_hard_cap();
    assert_eq!(
        raised.probe.expect("raised probe").adjustment,
        Some(FdAdjustment::Raised { from: SOFT })
    );
    assert_eq!(limits().rlim_cur, HARD);
    check_count(HARD);
    assert_eq!(
        pbfhogg::process::raise_nofile_to_hard_cap()
            .probe
            .expect("already raised probe")
            .adjustment,
        Some(FdAdjustment::AlreadyAtCap)
    );

    lower_soft();
    assert_eq!(
        policy::soft_nofile().expect("soft limit"),
        policy::Limit::Finite(SOFT)
    );
    assert_eq!(policy::soft_nofile().expect("soft limit").to_json(), SOFT);
    assert_eq!(policy::Nofile::RaiseToHard.to_json(), "raise_to_hard");
    policy::Nofile::RaiseToHard
        .apply()
        .expect("harness raises to restricted hard cap");
    policy::Nofile::RaiseToHard
        .apply()
        .expect("harness raise is idempotent");
    check_count(HARD);

    // Retain a descriptor above the lowered soft limit: it does not consume
    // a slot below that limit, and must not reduce the reported allowance.
    let source = File::open("/dev/null").expect("dup source");
    use std::os::fd::{AsRawFd, FromRawFd};
    let soft_fd = i32::try_from(SOFT).expect("fixture limit fits an fd");
    // SAFETY: duplicates a live descriptor into a fresh owned slot.
    let high = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, soft_fd) };
    assert!(high >= soft_fd);
    // SAFETY: successful fcntl returned a new descriptor owned only here.
    let high = unsafe { File::from_raw_fd(high) };
    lower_soft();
    check_count(SOFT);
    drop(high);
    drop(source);

    let mut held = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => held.push(file),
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                break;
            }
        }
    }
    assert_eq!(
        count_open_descriptors(SOFT),
        FdCount::Exhausted(libc::EMFILE)
    );
    let exhausted = current_nofile();
    assert_eq!(exhausted.descriptors, 0);
    assert_eq!(
        exhausted.probe.expect("exhausted probe").open,
        FdCount::Exhausted(libc::EMFILE)
    );
    drop(held);
    check_count(SOFT);
}

#[test]
fn real_descriptor_contract_in_restricted_child() {
    if limits().rlim_max < HARD {
        crate::common::skip("descriptor contract requires a hard RLIMIT_NOFILE of at least 64");
        return;
    }
    let scratch = crate::common::test_scratch();
    let mut cmd = Command::new(std::env::current_exe().expect("test executable"));
    crate::common::process::isolate(&mut cmd, &scratch);
    cmd.args([
        "--exact",
        "nofile::restricted_child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD, "1")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    // SAFETY: the hook uses only setrlimit and errno retrieval before exec.
    unsafe {
        cmd.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: SOFT,
                rlim_max: HARD,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    crate::guard::apply(&mut cmd);
    let mut child = crate::common::process::OwnedChild::spawn(&mut cmd, &scratch)
        .expect("launch restricted descriptor child");
    let output = child
        .wait(pbfhogg::TEST_CHILD_TIMEOUT)
        .expect("bounded child wait");
    assert!(
        output.status.success(),
        "{}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child must execute its contract"
    );
}
