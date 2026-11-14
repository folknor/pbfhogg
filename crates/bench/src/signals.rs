//! SIGINT and SIGTERM as a cancellation request. brokkr stops a
//! marker-stopped profiled call with one SIGTERM and captures its hotpath
//! report only if the program then unwinds and drops its guard (`brokkr man
//! bench hotpath-capture`), so the signal must cancel the run rather than end
//! the process. Both are blocked in every thread, taken by one waiter with
//! `sigwait`, and the first trips the invocation; the library then returns
//! its cancellation error through the ordinary path. A later signal is
//! ignored: brokkr's SIGKILL bounds a run that does not stop.

use pbfhogg::host::Invocation;
use std::sync::atomic::{AtomicI32, Ordering};

/// The first signal taken, 0 before any.
static FIRST: AtomicI32 = AtomicI32::new(0);

/// Block both signals in the calling thread and start the waiter. Call
/// before any other thread exists: one started earlier keeps the default
/// disposition and would be killed instead.
pub(crate) fn install(cancel: Invocation) -> std::io::Result<()> {
    let set = signal_set();
    // SAFETY: `set` is an initialised signal set; a null old-set pointer is
    // allowed.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "the process's signal waiter, which lives as long as the process"
    )]
    let spawned = std::thread::Builder::new()
        .name("bench-signals".to_owned())
        .spawn(move || wait(&set, &cancel));
    if let Err(e) = spawned {
        // Nobody would take the blocked signals: restore them so a signal
        // still ends the process.
        // SAFETY: as above.
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut()) };
        return Err(e);
    }
    Ok(())
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the zeroed set; adding two valid
    // signal numbers cannot fail.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        set
    }
}

fn wait(set: &libc::sigset_t, cancel: &Invocation) {
    loop {
        let mut signo: libc::c_int = 0;
        // SAFETY: `set` is initialised and `signo` is a valid out pointer.
        if unsafe { libc::sigwait(set, &raw mut signo) } != 0 {
            continue;
        }
        if FIRST
            .compare_exchange(0, signo, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            cancel.cancel();
        }
    }
}

/// The exit status of a run a signal cancelled: 130 for SIGINT, 143 for
/// SIGTERM; `None` when no signal arrived.
pub(crate) fn interrupted_status() -> Option<u8> {
    match FIRST.load(Ordering::Acquire) {
        0 => None,
        signo => Some(u8::try_from(128 + signo).unwrap_or(130)),
    }
}
