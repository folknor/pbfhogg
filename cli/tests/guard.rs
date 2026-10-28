//! The promise guard: every `pbfhogg` child this package starts runs under a
//! seccomp filter that enforces, for its whole life, the promise in
//! reference/cli-reference.md "Names pbfhogg leaves behind": the CLI starts
//! no other process, and never closes, replaces or changes the descriptor or
//! file-status flags of a descriptor it inherited.
//!
//! [`apply`] hands the child a lease: the write end of a pipe at the number
//! the pipe got in this process (above stdio), with `FD_CLOEXEC` cleared in
//! the child, as a caller handing pbfhogg a lock descriptor would. Its
//! number reaches the child in `PBFHOGG_TEST_LEASE_FD`. Then, between fork
//! and exec, it installs a filter whose violations kill the whole process
//! with `SIGSYS` (`SECCOMP_RET_KILL_PROCESS`), which `CliInvoker` turns into
//! a test failure naming the guard:
//!
//! - process creation: `fork`, `vfork`, and `clone` without `CLONE_THREAD`.
//!   `clone3` passes its flags in memory a filter cannot read, so it fails
//!   with `ENOSYS`; glibc then creates threads through `clone`, and a caller
//!   creating a process through it falls back to `clone` too and dies.
//! - the lease: `close`, `close_range` over it, `dup`/`dup2`/`dup3` from or
//!   onto it, `fcntl` with `F_SETFD`, `F_SETFL`, `F_DUPFD` or
//!   `F_DUPFD_CLOEXEC`, `ioctl` with `FIOCLEX`, `FIONCLEX`, `FIONBIO` or
//!   `FIOASYNC`.
//! - descriptor passing: `sendmsg`, `sendmmsg` and `pidfd_getfd`, anywhere.
//!
//! The filter is stricter than the promise in two places, because a filter
//! sees only syscall numbers and register arguments and cannot follow an
//! alias: any duplicate of the lease is refused (a duplicate shares the open
//! file description, so its file-status flags are the lease's), and so is
//! every `sendmsg`, `sendmmsg` and `pidfd_getfd` (each can make such a
//! duplicate, or hand the lease to a process that outlives pbfhogg).
//! pbfhogg needs none of them.
//!
//! What it does not prove: an io_uring submission (`IORING_OP_CLOSE` and the
//! like) is not a syscall the filter sees, so closing the lease through a
//! ring would pass; pbfhogg uses io_uring only for its own `--io-uring`
//! output writer, and the brokkr.toml textlint rules stay the tripwire
//! there. A caller that tries `clone3`, gets `ENOSYS`, ignores it and
//! carries on would pass as well: the guard proves that no process creation
//! succeeded, not that none was attempted. And it covers the runs the tests
//! make, not every command and flag combination.
//!
//! Hosts: Linux on little-endian x86_64 or aarch64, with
//! `SECCOMP_RET_KILL_PROCESS` (Linux 4.14). This process asks the kernel
//! once, without side effects, whether that action is available; where it is
//! not, children run unguarded and the control test skips. Where it is, a
//! filter the kernel rejects fails the spawn, so a broken program can never
//! quietly turn the guard off. The control test proves each rule fires, and
//! that the guard leaves alone what it must.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::Command;
use std::sync::OnceLock;

/// The variable naming the lease descriptor in the child.
pub const LEASE_FD_ENV: &str = "PBFHOGG_TEST_LEASE_FD";

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

// Classic BPF opcodes (linux/filter.h, linux/bpf_common.h).
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGT_K: u16 = 0x25;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;

// `struct seccomp_data`: nr, arch, instruction pointer, then six 64-bit
// arguments. Only the low word of each is read (little-endian), which is all
// the kernel uses of an fd, a command or a range bound.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const fn off_arg(i: u32) -> u32 {
    16 + 8 * i
}

const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
// The ENOSYS value is small and positive.
#[allow(clippy::cast_sign_loss, reason = "ENOSYS is a small positive errno")]
const ENOSYS_RET: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;

fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn offset(n: usize) -> u8 {
    u8::try_from(n).expect("a guard block fits a BPF jump")
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "syscall numbers are small"
)]
fn nr(sys: libc::c_long) -> u32 {
    sys as u32
}

/// Whether children can be guarded on this host.
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        if !cfg!(all(
            target_endian = "little",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )) {
            return false;
        }
        let action: u32 = KILL;
        // SAFETY: SECCOMP_GET_ACTION_AVAIL reads one u32 and changes nothing.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_GET_ACTION_AVAIL,
                0,
                &raw const action,
            )
        };
        rc == 0
    })
}

/// One syscall's block: when the syscall number is `sys`, run `body` (which
/// must end every path in a return), else fall through to the next block.
fn block(prog: &mut Vec<libc::sock_filter>, sys: libc::c_long, body: &[libc::sock_filter]) {
    prog.push(stmt(LD_W_ABS, OFF_NR));
    prog.push(jump(JEQ_K, nr(sys), 0, offset(body.len())));
    prog.extend_from_slice(body);
}

/// Kill when any of `args` holds `lease`.
fn any_arg_is(lease: u32, args: &[u32]) -> Vec<libc::sock_filter> {
    // Layout: (load, test) per argument, then ALLOW, then KILL.
    let mut body = Vec::new();
    let kill_at = args.len() * 2 + 1;
    for (i, &arg) in args.iter().enumerate() {
        let test_at = i * 2 + 1;
        body.push(stmt(LD_W_ABS, off_arg(arg)));
        body.push(jump(JEQ_K, lease, offset(kill_at - test_at - 1), 0));
    }
    body.push(stmt(RET_K, ALLOW));
    body.push(stmt(RET_K, KILL));
    body
}

/// Kill when argument 0 is `lease` and argument 1 is one of `ops`.
fn op_on(lease: u32, ops: &[u32]) -> Vec<libc::sock_filter> {
    // 0 load fd; 1 not the lease: to ALLOW; 2 load op; tests; ALLOW; KILL.
    let allow_at = 3 + ops.len();
    let kill_at = allow_at + 1;
    let mut body = vec![
        stmt(LD_W_ABS, off_arg(0)),
        jump(JEQ_K, lease, 0, offset(allow_at - 2)),
        stmt(LD_W_ABS, off_arg(1)),
    ];
    for (i, &op) in ops.iter().enumerate() {
        let test_at = 3 + i;
        body.push(jump(JEQ_K, op, offset(kill_at - test_at - 1), 0));
    }
    body.push(stmt(RET_K, ALLOW));
    body.push(stmt(RET_K, KILL));
    body
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "small positive constants"
)]
fn program(lease: u32) -> Vec<libc::sock_filter> {
    let kill = [stmt(RET_K, KILL)];
    let mut prog = vec![
        stmt(LD_W_ABS, OFF_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, KILL),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        // The x32 ABI numbers its syscalls from bit 30.
        prog.push(stmt(LD_W_ABS, OFF_NR));
        prog.push(jump(JGE_K, 0x4000_0000, 0, 1));
        prog.push(stmt(RET_K, KILL));
        block(&mut prog, libc::SYS_fork, &kill);
        block(&mut prog, libc::SYS_vfork, &kill);
        block(&mut prog, libc::SYS_dup2, &any_arg_is(lease, &[0, 1]));
    }
    block(
        &mut prog,
        libc::SYS_clone,
        &[
            stmt(LD_W_ABS, off_arg(0)),
            jump(JSET_K, libc::CLONE_THREAD as u32, 0, 1),
            stmt(RET_K, ALLOW),
            stmt(RET_K, KILL),
        ],
    );
    block(&mut prog, libc::SYS_clone3, &[stmt(RET_K, ENOSYS_RET)]);
    block(&mut prog, libc::SYS_sendmsg, &kill);
    block(&mut prog, libc::SYS_sendmmsg, &kill);
    block(&mut prog, libc::SYS_pidfd_getfd, &kill);
    block(&mut prog, libc::SYS_close, &any_arg_is(lease, &[0]));
    block(&mut prog, libc::SYS_dup, &any_arg_is(lease, &[0]));
    block(&mut prog, libc::SYS_dup3, &any_arg_is(lease, &[0, 1]));
    block(
        &mut prog,
        libc::SYS_fcntl,
        &op_on(
            lease,
            &[
                libc::F_SETFD as u32,
                libc::F_SETFL as u32,
                libc::F_DUPFD as u32,
                libc::F_DUPFD_CLOEXEC as u32,
            ],
        ),
    );
    block(
        &mut prog,
        libc::SYS_ioctl,
        &op_on(
            lease,
            &[
                libc::FIOCLEX as u32,
                libc::FIONCLEX as u32,
                libc::FIONBIO as u32,
                libc::FIOASYNC as u32,
            ],
        ),
    );
    // close_range(first, last): kill when first <= lease <= last.
    block(
        &mut prog,
        libc::SYS_close_range,
        &[
            stmt(LD_W_ABS, off_arg(0)),
            jump(JGT_K, lease, 2, 0),
            stmt(LD_W_ABS, off_arg(1)),
            jump(JGE_K, lease, 1, 0),
            stmt(RET_K, ALLOW),
            stmt(RET_K, KILL),
        ],
    );
    prog.push(stmt(RET_K, ALLOW));
    prog
}

/// Put `cmd` under the guard, when the host supports it. Register this after
/// every other `pre_exec` hook: hooks run in order, and nothing may run in
/// the child after the filter but the exec.
pub fn apply(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    if !supported() {
        return;
    }
    let (reader, writer) = std::io::pipe().expect("lease pipe");
    // Nobody reads: the lease is only ever held, never written.
    drop(reader);
    let writer = OwnedFd::from(writer);
    let lease: RawFd = writer.as_raw_fd();
    assert!(lease > 2, "the lease must not be a stdio descriptor");
    let lease_k = u32::try_from(lease).expect("descriptor numbers are non-negative");
    let filter = program(lease_k);
    cmd.env(LEASE_FD_ENV, lease.to_string());
    // SAFETY: the closure runs between fork and exec, calls only
    // async-signal-safe syscalls (`fcntl`, `prctl`, `seccomp`) and allocates
    // nothing: the filter is built above, before the fork.
    unsafe {
        cmd.pre_exec(move || {
            // Owned here so the parent's copy lives until the Command drops,
            // and the number is never reused while a child may be forking.
            let fd = writer.as_raw_fd();
            if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let prog = libc::sock_fprog {
                len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
                filter: filter.as_ptr().cast_mut(),
            };
            if libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &raw const prog,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Whether `status` is a death by the guard's `SIGSYS`.
pub fn killed_by_guard(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGSYS)
}

mod tests {
    //! The control: re-run this test binary's own [`probe`] under the guard,
    //! once per case, and check that every rule fires and nothing else does.

    use super::{LEASE_FD_ENV, apply, killed_by_guard, supported};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const PROBE_ENV: &str = "PBFHOGG_TEST_GUARD_PROBE";
    const PROBE_NAME: &str = "guard::tests::probe";

    fn lease() -> libc::c_int {
        std::env::var(LEASE_FD_ENV)
            .expect("the guard names the lease")
            .parse()
            .expect("a descriptor number")
    }

    /// A fresh pipe's read end, close-on-exec, owned by nobody.
    fn spare() -> libc::c_int {
        let mut fds = [0; 2];
        // SAFETY: a two-element buffer for pipe2.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        fds[0]
    }

    /// Fail the probe when a call the guard must allow failed.
    fn ok(what: &str, rc: libc::c_long) {
        assert!(
            rc >= 0,
            "{what} failed: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Fork, and end a child the guard failed to stop at once.
    fn forked(pid: libc::c_long) {
        if pid == 0 {
            // SAFETY: _exit is always safe and ends only the child.
            unsafe { libc::_exit(0) }
        }
    }

    fn send_lease(n: libc::c_int) {
        let mut pair = [0; 2];
        // SAFETY: a two-element buffer for socketpair.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        ok("socketpair", rc.into());
        let mut byte = [0_u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a one-descriptor control message"
        )]
        // SAFETY: CMSG_SPACE is a pure size computation.
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
        let mut control = vec![0_u8; space];
        // SAFETY: an all-zero msghdr is valid; the fields set below point at
        // live buffers, and the cmsg pointer lies inside `control`.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            #[allow(
                clippy::useless_conversion,
                reason = "the field is usize on glibc, u32 on musl"
            )]
            {
                msg.msg_controllen = space.try_into().expect("a small control buffer");
            }
            let cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            #[allow(clippy::cast_possible_truncation, reason = "one descriptor")]
            {
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            }
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), n);
            libc::sendmsg(pair[0], &raw const msg, 0);
        }
    }

    /// One violation or one allowed operation, chosen by the environment.
    /// Without it this is an ordinary, empty test.
    #[test]
    fn probe() {
        let Ok(case) = std::env::var(PROBE_ENV) else {
            return;
        };
        println!("probe {case} start");
        let n = lease();
        let n_long = libc::c_long::from(n);
        // SAFETY: raw syscalls on descriptor numbers; each violation is meant
        // to die at the syscall's entry, and each allowed one is checked.
        unsafe {
            match case.as_str() {
                "fork" => forked(libc::fork().into()),
                #[cfg(target_arch = "x86_64")]
                "raw_fork" => forked(libc::syscall(libc::SYS_fork)),
                #[cfg(target_arch = "x86_64")]
                "raw_vfork" => forked(libc::syscall(libc::SYS_vfork)),
                "close" => {
                    libc::close(n);
                }
                "dup" => {
                    libc::dup(n);
                }
                #[cfg(target_arch = "x86_64")]
                "dup2_from" => {
                    libc::syscall(libc::SYS_dup2, n_long, libc::c_long::from(spare()));
                }
                #[cfg(target_arch = "x86_64")]
                "dup2_onto" => {
                    libc::syscall(libc::SYS_dup2, libc::c_long::from(spare()), n_long);
                }
                "dup3_from" => {
                    libc::dup3(n, spare(), 0);
                }
                "dup3_onto" => {
                    libc::dup3(spare(), n, 0);
                }
                "setfd" => {
                    libc::fcntl(n, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                "setfl" => {
                    libc::fcntl(n, libc::F_SETFL, libc::O_NONBLOCK);
                }
                "dupfd" => {
                    libc::fcntl(n, libc::F_DUPFD, 0);
                }
                "dupfd_cloexec" => {
                    libc::fcntl(n, libc::F_DUPFD_CLOEXEC, 0);
                }
                "fioclex" => {
                    libc::ioctl(n, libc::FIOCLEX);
                }
                "fionclex" => {
                    libc::ioctl(n, libc::FIONCLEX);
                }
                "fionbio" => {
                    let on: libc::c_int = 1;
                    libc::ioctl(n, libc::FIONBIO, &raw const on);
                }
                "fioasync" => {
                    let on: libc::c_int = 1;
                    libc::ioctl(n, libc::FIOASYNC, &raw const on);
                }
                "close_range_exact" => {
                    libc::syscall(libc::SYS_close_range, n_long, n_long, 0);
                }
                "close_range_upward" => {
                    libc::syscall(
                        libc::SYS_close_range,
                        n_long,
                        libc::c_long::from(u32::MAX),
                        0,
                    );
                }
                "close_range_ending_at" => {
                    libc::syscall(libc::SYS_close_range, n_long - 1, n_long, 0);
                }
                "thread_close" => {
                    std::thread::spawn(move || {
                        libc::close(n);
                    })
                    .join()
                    .ok();
                }
                "sendmsg" => send_lease(n),
                "sendmmsg" => {
                    // Raw: musl's wrapper returns early for an empty vector.
                    libc::syscall(libc::SYS_sendmmsg, -1, std::ptr::null::<u8>(), 0, 0);
                }
                "pidfd_getfd" => {
                    libc::syscall(libc::SYS_pidfd_getfd, -1, n_long, 0);
                }
                "allowed" => allowed(n),
                other => panic!("unknown probe case {other}"),
            }
        }
        println!("probe {case} survived");
    }

    /// Everything the guard must leave alone: the lease as inherited, reads
    /// of its flags, threads, and every guarded call on other descriptors.
    unsafe fn allowed(n: libc::c_int) {
        // SAFETY: raw syscalls on descriptors this probe owns, and flag reads.
        unsafe {
            // The lease arrived open, as a pipe, without close-on-exec.
            let mut st: libc::stat = std::mem::zeroed();
            ok("fstat lease", libc::fstat(n, &raw mut st).into());
            assert_eq!(
                st.st_mode & libc::S_IFMT,
                libc::S_IFIFO,
                "the lease is a pipe"
            );
            assert_eq!(libc::fcntl(n, libc::F_GETFD), 0, "the lease is inherited");
            let fl = libc::fcntl(n, libc::F_GETFL);
            assert_eq!(
                fl & libc::O_ACCMODE,
                libc::O_WRONLY,
                "the lease is the write end"
            );
            std::thread::spawn(|| ())
                .join()
                .expect("a thread starts and ends");
            let r = libc::syscall(libc::SYS_clone3, std::ptr::null::<u8>(), 0);
            assert_eq!(r, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSYS),
                "clone3 is refused with ENOSYS"
            );
            let m = spare();
            ok(
                "F_SETFL",
                libc::fcntl(m, libc::F_SETFL, libc::O_NONBLOCK).into(),
            );
            ok(
                "F_SETFD",
                libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC).into(),
            );
            let on: libc::c_int = 1;
            ok(
                "FIONBIO",
                libc::ioctl(m, libc::FIONBIO, &raw const on).into(),
            );
            ok("FIOCLEX", libc::ioctl(m, libc::FIOCLEX).into());
            let d = libc::dup(m);
            ok("dup", d.into());
            ok("dup3", libc::dup3(m, d, libc::O_CLOEXEC).into());
            #[cfg(target_arch = "x86_64")]
            ok(
                "dup2",
                libc::syscall(libc::SYS_dup2, libc::c_long::from(m), libc::c_long::from(d)),
            );
            ok(
                "close_range",
                libc::syscall(
                    libc::SYS_close_range,
                    libc::c_long::from(d),
                    libc::c_long::from(d),
                    0,
                ),
            );
            // A range wholly above the lease and one below it. Single
            // descriptors only: a wider range could close this process's own.
            let above = libc::fcntl(m, libc::F_DUPFD_CLOEXEC, n + 1);
            ok("F_DUPFD_CLOEXEC", above.into());
            ok(
                "close_range above",
                libc::syscall(
                    libc::SYS_close_range,
                    libc::c_long::from(above),
                    libc::c_long::from(above),
                    0,
                ),
            );
            let below = libc::dup(m);
            ok("dup below", below.into());
            ok(
                "close_range below",
                libc::syscall(
                    libc::SYS_close_range,
                    libc::c_long::from(below),
                    libc::c_long::from(below),
                    0,
                ),
            );
            ok("close", libc::close(m).into());
            let mut pair = [0; 2];
            ok(
                "socketpair",
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
                .into(),
            );
            ok(
                "write",
                libc::write(pair[0], b"x".as_ptr().cast(), 1) as libc::c_long,
            );
        }
    }

    struct Ran {
        status: std::process::ExitStatus,
        out: String,
    }

    fn run_probe(case: &str) -> Ran {
        let dir = crate::common::test_scratch();
        let out_path = dir.path().join("out");
        let err_path = dir.path().join("err");
        let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
        cmd.args([PROBE_NAME, "--exact", "--test-threads", "1", "--nocapture"])
            .env(PROBE_ENV, case)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&out_path).expect("stdout file"))
            .stderr(std::fs::File::create(&err_path).expect("stderr file"));
        apply(&mut cmd);
        let mut child = cmd.spawn().expect("spawn the probe");
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait for the probe") {
                break status;
            }
            if start.elapsed() > Duration::from_secs(10) {
                child.kill().ok();
                child.wait().ok();
                panic!("probe {case} did not finish");
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        let mut out = std::fs::read_to_string(&out_path).unwrap_or_default();
        out.push_str(&std::fs::read_to_string(&err_path).unwrap_or_default());
        Ran { status, out }
    }

    /// Every rule kills the probe at its violation, and the allowed case,
    /// which exercises each rule's neighbours, runs to the end.
    #[test]
    fn the_promise_guard_kills_every_violation_and_nothing_else() {
        if !supported() {
            eprintln!("skipped: this host cannot run the promise guard");
            return;
        }
        let mut killing = vec![
            "fork",
            "close",
            "dup",
            "dup3_from",
            "dup3_onto",
            "setfd",
            "setfl",
            "dupfd",
            "dupfd_cloexec",
            "fioclex",
            "fionclex",
            "fionbio",
            "fioasync",
            "close_range_exact",
            "close_range_upward",
            "close_range_ending_at",
            "thread_close",
            "sendmsg",
            "sendmmsg",
            "pidfd_getfd",
        ];
        if cfg!(target_arch = "x86_64") {
            killing.extend(["raw_fork", "raw_vfork", "dup2_from", "dup2_onto"]);
        }
        let mut failures = Vec::new();
        for case in killing {
            let ran = run_probe(case);
            let started = ran.out.contains(&format!("probe {case} start"));
            let survived = ran.out.contains(&format!("probe {case} survived"));
            if !(killed_by_guard(ran.status) && started && !survived) {
                failures.push(format!(
                    "{case}: {} (started {started}, survived {survived})\n{}",
                    ran.status, ran.out
                ));
            }
        }
        let ran = run_probe("allowed");
        if !(ran.status.success() && ran.out.contains("probe allowed survived")) {
            failures.push(format!("allowed: {}\n{}", ran.status, ran.out));
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }
}
