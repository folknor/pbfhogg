//! LD_PRELOAD recorder for the CLI's process and shared-host policy calls.
#![cfg(all(target_os = "linux", target_env = "gnu"))]
#![allow(clippy::missing_safety_doc, clippy::cast_possible_truncation)]

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

static FD: AtomicI32 = AtomicI32::new(-1);
static NEXT: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static ID: Cell<u64> = const { Cell::new(0) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

unsafe fn symbol(name: &CStr) -> *mut c_void {
    unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) }
}

extern "C" fn init() {
    unsafe {
        let getenv: unsafe extern "C" fn(*const c_char) -> *mut c_char =
            std::mem::transmute(symbol(c"getenv"));
        let path = getenv(c"PROCESS_TRACE_PATH".as_ptr());
        if !path.is_null() {
            FD.store(
                libc::open(
                    path,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
                    0o644,
                ),
                Ordering::Relaxed,
            );
        }
    }
}
#[used]
#[unsafe(link_section = ".init_array")]
static INIT: extern "C" fn() = init;

struct Line {
    bytes: [u8; 4352],
    len: usize,
}
impl Line {
    fn new() -> Self {
        Self {
            bytes: [0; 4352],
            len: 0,
        }
    }
    fn text(&mut self, bytes: &[u8]) {
        let n = bytes.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }
    fn number(&mut self, n: i128) {
        let mut digits = [0; 40];
        let mut at = digits.len();
        let mut value = n.unsigned_abs();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        if n < 0 {
            self.text(b"-");
        }
        self.text(&digits[at..]);
    }
    fn field(&mut self, n: i128) {
        self.text(b" ");
        self.number(n);
    }
    fn limit(&mut self, n: libc::rlim_t) {
        self.text(b" ");
        if n == libc::RLIM_INFINITY {
            self.text(b"inf");
        } else {
            self.number(i128::from(n));
        }
    }
    fn error(&mut self, failed: bool, errno: c_int) {
        if failed {
            self.field(i128::from(errno));
        } else {
            self.text(b" -");
        }
    }
}
fn record(op: impl FnOnce(&mut Line)) {
    // Preserve errno before any TLS initialisation or formatting.
    let errno = unsafe { *libc::__errno_location() };
    let fd = FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    BUSY.with(|busy| {
        if busy.replace(true) {
            return;
        }
        unsafe {
            let mut line = Line::new();
            ID.with(|id| line.number(i128::from(id.get())));
            op(&mut line);
            line.text(b"\n");
            libc::write(fd, line.bytes.as_ptr().cast(), line.len);
        }
        busy.set(false);
    });
    unsafe {
        *libc::__errno_location() = errno;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mallopt(param: c_int, value: c_int) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(c_int, c_int) -> c_int =
            std::mem::transmute(symbol(c"mallopt"));
        let ret = real(param, value);
        record(|l| {
            l.text(b" M");
            l.field(param.into());
            l.field(value.into());
            l.field(ret.into());
        });
        ret
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    unsafe {
        let real: unsafe extern "C" fn(*const c_char) -> *mut c_char =
            std::mem::transmute(symbol(c"getenv"));
        let ret = real(name);
        record(|l| {
            l.text(b" V ");
            l.text(CStr::from_ptr(name).to_bytes());
            l.field(i128::from(!ret.is_null()));
        });
        ret
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_getaffinity(
    pid: libc::pid_t,
    size: usize,
    mask: *mut libc::cpu_set_t,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(libc::pid_t, usize, *mut libc::cpu_set_t) -> c_int =
            std::mem::transmute(symbol(c"sched_getaffinity"));
        let ret = real(pid, size, mask);
        record(|l| {
            l.text(b" A");
            l.field(ret.into());
            l.field(if ret == 0 {
                libc::CPU_COUNT(&*mask).into()
            } else {
                0
            });
        });
        ret
    }
}

struct Start {
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    id: u64,
}
extern "C" fn trampoline(arg: *mut c_void) -> *mut c_void {
    let start = unsafe { Box::from_raw(arg.cast::<Start>()) };
    ID.with(|id| id.set(start.id));
    record(|l| l.text(b" S"));
    let ret = (start.start)(start.arg);
    record(|l| l.text(b" E"));
    ret
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(
            *mut libc::pthread_t,
            *const libc::pthread_attr_t,
            extern "C" fn(*mut c_void) -> *mut c_void,
            *mut c_void,
        ) -> c_int = std::mem::transmute(symbol(c"pthread_create"));
        if FD.load(Ordering::Relaxed) < 0 {
            return real(thread, attr, start, arg);
        }
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let state = Box::into_raw(Box::new(Start { start, arg, id }));
        let ret = real(thread, attr, trampoline, state.cast());
        if ret != 0 {
            drop(Box::from_raw(state));
        }
        record(|l| {
            l.text(b" C");
            if ret == 0 {
                l.field(id.into());
            } else {
                l.text(b" -");
            }
            l.field(ret.into());
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrlimit(
    resource: libc::__rlimit_resource_t,
    limits: *mut libc::rlimit,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(libc::__rlimit_resource_t, *mut libc::rlimit) -> c_int =
            std::mem::transmute(symbol(c"getrlimit"));
        let ret = real(resource, limits);
        let errno = *libc::__errno_location();
        record(|l| {
            l.text(b" GR");
            l.field(resource.into());
            l.field(ret.into());
            l.error(ret != 0, errno);
            if ret == 0 {
                l.limit((*limits).rlim_cur);
                l.limit((*limits).rlim_max);
            } else {
                l.text(b" - -");
            }
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setrlimit(
    resource: libc::__rlimit_resource_t,
    limits: *const libc::rlimit,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(libc::__rlimit_resource_t, *const libc::rlimit) -> c_int =
            std::mem::transmute(symbol(c"setrlimit"));
        let ret = real(resource, limits);
        let errno = *libc::__errno_location();
        record(|l| {
            l.text(b" SR");
            l.field(resource.into());
            l.limit((*limits).rlim_cur);
            l.limit((*limits).rlim_max);
            l.field(ret.into());
            l.error(ret != 0, errno);
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrlimit64(
    resource: libc::__rlimit_resource_t,
    limits: *mut libc::rlimit64,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(libc::__rlimit_resource_t, *mut libc::rlimit64) -> c_int =
            std::mem::transmute(symbol(c"getrlimit64"));
        let ret = real(resource, limits);
        let errno = *libc::__errno_location();
        record(|l| {
            l.text(b" GR");
            l.field(resource.into());
            l.field(ret.into());
            l.error(ret != 0, errno);
            if ret == 0 {
                l.limit((*limits).rlim_cur);
                l.limit((*limits).rlim_max);
            } else {
                l.text(b" - -");
            }
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setrlimit64(
    resource: libc::__rlimit_resource_t,
    limits: *const libc::rlimit64,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(libc::__rlimit_resource_t, *const libc::rlimit64) -> c_int =
            std::mem::transmute(symbol(c"setrlimit64"));
        let ret = real(resource, limits);
        let errno = *libc::__errno_location();
        record(|l| {
            l.text(b" SR");
            l.field(resource.into());
            l.limit((*limits).rlim_cur);
            l.limit((*limits).rlim_max);
            l.field(ret.into());
            l.error(ret != 0, errno);
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_fadvise(
    fd: c_int,
    offset: libc::off_t,
    len: libc::off_t,
    advice: c_int,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(c_int, libc::off_t, libc::off_t, c_int) -> c_int =
            std::mem::transmute(symbol(c"posix_fadvise"));
        let ret = real(fd, offset, len, advice);
        record(|l| {
            l.text(b" F ");
            let mut link = Line::new();
            link.text(b"/proc/self/fd/");
            link.number(fd.into());
            link.text(&[0]);
            let mut path = [0u8; 4096];
            let n = libc::syscall(
                libc::SYS_readlink,
                link.bytes.as_ptr(),
                path.as_mut_ptr(),
                path.len(),
            );
            if let Ok(n) = usize::try_from(n) {
                l.text(&path[..n]);
            } else {
                l.text(b"?");
            }
            l.field(offset.into());
            l.field(len.into());
            l.field(advice.into());
            l.field(ret.into());
        });
        ret
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_fadvise64(
    fd: c_int,
    offset: libc::off64_t,
    len: libc::off64_t,
    advice: c_int,
) -> c_int {
    unsafe {
        let real: unsafe extern "C" fn(c_int, libc::off64_t, libc::off64_t, c_int) -> c_int =
            std::mem::transmute(symbol(c"posix_fadvise64"));
        let ret = real(fd, offset, len, advice);
        record(|l| {
            l.text(b" F ");
            let mut link = Line::new();
            link.text(b"/proc/self/fd/");
            link.number(fd.into());
            link.text(&[0]);
            let mut path = [0u8; 4096];
            let n = libc::syscall(
                libc::SYS_readlink,
                link.bytes.as_ptr(),
                path.as_mut_ptr(),
                path.len(),
            );
            if let Ok(n) = usize::try_from(n) {
                l.text(&path[..n]);
            } else {
                l.text(b"?");
            }
            l.field(offset.into());
            l.field(len.into());
            l.field(advice.into());
            l.field(ret.into());
        });
        ret
    }
}
