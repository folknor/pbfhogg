use std::os::unix::io::RawFd;
use std::path::Path;

use crate::error::DevError;

/// RAII lock guard. Releases the flock and closes the fd on drop.
pub struct LockGuard {
    fd: RawFd,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

/// Acquire an exclusive non-blocking lock on `{dir}/.dev.lock`.
///
/// On success, writes the current PID to the lock file.
/// On `EWOULDBLOCK`, reads the file to report which PID holds the lock.
pub fn acquire(dir: &Path) -> Result<LockGuard, DevError> {
    let lock_path = dir.join(".dev.lock");
    let c_path = path_to_cstring(&lock_path)?;
    let fd = open_lock_file(&c_path)?;

    match try_flock(fd) {
        Ok(()) => {
            write_pid(fd);
            Ok(LockGuard { fd })
        }
        Err(held_by) => {
            // flock failed — close the fd before returning the error.
            unsafe { libc::close(fd) };
            Err(held_by)
        }
    }
}

/// Open (or create) the lock file, returning the raw fd.
fn open_lock_file(c_path: &std::ffi::CString) -> Result<RawFd, DevError> {
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
            0o644,
        )
    };

    if fd < 0 {
        return Err(DevError::Lock(format!(
            "failed to open lock file: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(fd)
}

/// Try a non-blocking exclusive flock. Returns `Ok(())` on success, or a
/// `DevError::Lock` describing the holder on `EWOULDBLOCK`.
fn try_flock(fd: RawFd) -> Result<(), DevError> {
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if ret == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        let pid = read_holder_pid(fd);
        Err(DevError::Lock(format!("already locked by PID {pid}")))
    } else {
        Err(DevError::Lock(format!("flock failed: {err}")))
    }
}

/// Read the existing file contents to discover the PID of the current holder.
fn read_holder_pid(fd: RawFd) -> String {
    let mut buf = [0u8; 32];

    // Seek to start before reading.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return "unknown".to_owned();
    }

    // n is positive here, so the cast is safe.
    let len: usize = match usize::try_from(n) {
        Ok(v) => v,
        Err(_) => return "unknown".to_owned(),
    };

    let s = String::from_utf8_lossy(&buf[..len]);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Truncate the lock file and write the current PID.
fn write_pid(fd: RawFd) {
    let pid = std::process::id().to_string();

    unsafe {
        libc::ftruncate(fd, 0);
        libc::lseek(fd, 0, libc::SEEK_SET);
        libc::write(fd, pid.as_ptr().cast(), pid.len());
    }
}

/// Convert a `Path` to a `CString`.
fn path_to_cstring(path: &Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        DevError::Lock(format!(
            "lock path contains nul byte: {}",
            path.display()
        ))
    })
}
