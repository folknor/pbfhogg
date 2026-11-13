//! Own-process test: subreaper state and adopted descendants cannot be shared
//! with libtest or another test. Fixtures are this Rust executable, never sh.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "../src/process.rs"]
mod process;
#[allow(dead_code)]
#[path = "../src/tunables.rs"]
mod tunables;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn descendant(exe: &Path, mode: &str) -> std::process::Child {
    let mut child = Command::new(exe);
    child
        .env_clear()
        .args(["--exact", "fixture", "--ignored", "--nocapture"])
        .env("PBFHOGG_LIBRARY_FIXTURE", mode);
    child.spawn().unwrap()
}
#[expect(
    clippy::zombie_processes,
    reason = "fixtures deliberately leave descendants for the supervisor to reap"
)]
fn fixture_body(mode: &str, exe: &Path) {
    if let Some(path) = mode.strip_prefix("fifo:") {
        let mut fifo = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let bytes = [b'm'; 8192];
        for _ in 0..(tunables::CAPTURE_LIMIT * 2 / bytes.len()) {
            fifo.write_all(&bytes).unwrap();
        }
        return;
    }
    match mode {
        "quiet" => {}
        "parked" => std::thread::park(),
        "orphan" => {
            let _child = descendant(exe, "parked");
        }
        "chain" => {
            let mut child = descendant(exe, "orphan");
            assert!(child.wait().unwrap().success());
        }
        "zombie" => {
            let child = descendant(exe, "quiet");
            // Wait for the kernel's exit notification without reaping: the
            // worker must really leave a zombie, not merely a live child.
            let fd = open_pidfd(child.id());
            let mut event = libc::pollfd {
                fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one writable pollfd, held open by fd.
            // wait-deliberate: pidfd readiness observes exit; the parent's driver deadline kills this fixture on a hang.
            assert_eq!(unsafe { libc::poll(&mut event, 1, -1) }, 1);
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", child.id())).unwrap();
            assert!(stat.contains(") Z "));
        }
        "flood" => {
            let buf = [b'x'; 8192];
            loop {
                std::io::stdout().write_all(&buf).unwrap();
                std::io::stderr().write_all(&buf).unwrap();
            }
        }
        _ => panic!("unknown fixture {mode}"),
    }
}
#[test]
#[ignore = "subprocess fixture, activated only by its parent"]
fn fixture() {
    if let Ok(mode) = std::env::var("PBFHOGG_LIBRARY_FIXTURE") {
        fixture_body(&mode, &std::env::current_exe().unwrap());
    }
}

#[test]
fn supervisor_lifecycle() {
    let exe = std::env::current_exe().unwrap();
    let _bound = pbfhogg_test_scratch::HangBound::arm(
        tunables::TEST_HANG_BOUND,
        tunables::TEST_REPORT_GRACE,
        "library process lifecycle",
    );
    process::subreaper().unwrap();
    // loop-ends: finite fixture matrix; each worker is supervised under its own deadline.
    for (mode, timeout, residual) in [
        ("quiet", false, false),
        ("orphan", false, true),
        ("chain", false, true),
        ("zombie", false, true),
        ("parked", true, false),
        ("flood", true, false),
    ] {
        let mut cmd = process::command(&exe);
        cmd.args(["--exact", "fixture", "--ignored", "--nocapture"])
            .env("PBFHOGG_LIBRARY_FIXTURE", mode);
        let deadline = if timeout {
            tunables::TEST_KILL_DEADLINE
        } else {
            tunables::TEST_WORKER_DEADLINE
        };
        let result = process::run(cmd, deadline, None).unwrap();
        assert_eq!(result.timed_out, timeout, "{mode}: {result:?}");
        assert_eq!(result.residual_children, residual, "{mode}: {result:?}");
        if timeout {
            assert!(result.elapsed >= deadline);
        } else {
            assert!(result.status.success(), "{mode}: {result:?}");
        }
        if mode == "flood" {
            assert!(result.out.overflow && result.err.overflow);
            assert_eq!(result.out.bytes.len(), tunables::CAPTURE_LIMIT);
            assert_eq!(result.err.bytes.len(), tunables::CAPTURE_LIMIT);
        }
        assert!(result.markers.bytes.is_empty());
        let mut status = 0;
        // SAFETY: writable status; this process owns every test child.
        assert_eq!(
            unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    fifo_capture(&exe);
    let mut command = process::command(&exe);
    command
        .args(["--exact", "fixture", "--ignored", "--nocapture"])
        .env("PBFHOGG_LIBRARY_FIXTURE", "quiet");
    let result = process::run(command, std::time::Duration::ZERO, None).unwrap();
    assert!(result.timed_out && !result.residual_children);
}

fn open_pidfd(pid: u32) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    // SAFETY: scalar PID and flags; a successful fd is newly owned.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(fd >= 0);
    // SAFETY: fresh fd from pidfd_open.
    unsafe { std::os::fd::OwnedFd::from_raw_fd(i32::try_from(fd).unwrap()) }
}

fn fifo_capture(exe: &Path) {
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    let scratch = pbfhogg_test_scratch::scratch();
    let path = scratch.path().join("markers");
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated path, mode specifies private access.
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    let fifo = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();
    let mut cmd = process::command(exe);
    cmd.args(["--exact", "fixture", "--ignored", "--nocapture"])
        .env(
            "PBFHOGG_LIBRARY_FIXTURE",
            format!("fifo:{}", path.display()),
        );
    let result = process::run(cmd, tunables::TEST_WORKER_DEADLINE, Some(fifo)).unwrap();
    assert!(result.status.success() && !result.timed_out && !result.residual_children);
    assert!(result.markers.overflow);
    assert_eq!(result.markers.bytes, vec![b'm'; tunables::CAPTURE_LIMIT]);
}
