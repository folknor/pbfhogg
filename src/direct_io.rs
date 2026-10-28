//! The one owner of `O_DIRECT`: opening a file for direct I/O, and the
//! alignment every direct read or write of that file must keep.
//!
//! Direct I/O requires the buffer address, the file offset and the transfer
//! length to be aligned. The requirement belongs to the device and the
//! filesystem, and Linux 6.1+ reports it per file through
//! `statx(STATX_DIOALIGN)`. An `O_DIRECT` open succeeds whatever the
//! requirement is; a transfer that misses it fails later with a bare `EINVAL`.
//! So the requirement is discovered here, once per open, and every direct
//! backend (`DirectWriter`, the `io_uring` writer, `DirectReader`,
//! `DirectPositional`) carries the value it was handed instead of assuming
//! one.
//!
//! The alignment used is never below [`DIO_ALIGN_FLOOR`] (4096): a host
//! reporting 512 gets exactly the I/O it got before discovery existed, and
//! only a requirement above the floor changes anything. Where nothing usable
//! is reported - a kernel before 6.1, a filesystem that does not fill the
//! field, a failed `statx`, a non-glibc target (the `libc` crate exposes
//! `statx` on glibc only), or a field of zero - the floor is used as before.
//! A zero field is not proof that no direct I/O happens: f2fs reports zero
//! when only its writes fall back to buffered I/O, and ext4 reports zero for
//! inline-data files that a write may later convert to extents. The floor is
//! the compatibility answer for those, not a guarantee; one query per open
//! cannot follow a file whose eligibility changes as it grows, and querying
//! per transfer would put a syscall on the measured write path.
//!
//! A reported requirement this design cannot keep - not a power of two, or
//! larger than the [`IO_BUF_BYTES`] transfer buffer every backend reads and
//! writes whole - is refused at open as a `direct-io` platform refusal
//! (exit 3), rather than failing mid-run with `EINVAL`. A writer has already
//! created (and truncated) its file by then; an `Output` stages that file,
//! a public path constructor's caller sees it truncated.
//!
//! The `O_DIRECT` flag is spelled in this module only (a `textlint` rule in
//! `brokkr.toml` holds that), so no backend can open a direct file without
//! learning its alignment.

/// Lowest alignment any direct transfer uses. Every alignment Linux has
/// reported for a regular file below 4 KiB (512-byte sectors) divides it, so
/// a host whose requirement is at or below it keeps the I/O shape it had
/// before alignment discovery existed. The shared transfer size is asserted a
/// multiple of it in `tunables`.
pub(crate) const DIO_ALIGN_FLOOR: usize = 4096;

#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
pub(crate) use imp::*;

#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::DIO_ALIGN_FLOOR;
    use crate::error::{Error, Result};
    use crate::tunables::IO_BUF_BYTES;

    /// What a file reported for one direction of direct I/O: the buffer
    /// address alignment and the offset-and-length alignment. Zero means the
    /// field gave no requirement.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Reported {
        pub(super) mem: u32,
        pub(super) offset: u32,
    }

    /// The direction a file is opened for. Reads may report a smaller
    /// offset alignment than writes (`STATX_DIO_READ_ALIGN`, Linux 6.14+;
    /// XFS copy-on-write inodes do).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Access {
        #[cfg(feature = "linux-direct-io")]
        Read,
        Write,
    }

    /// Create (or truncate) `path` for `O_DIRECT` writing, and return it with
    /// the alignment its writes must keep. The direct and `io_uring` output
    /// backends' one create, at the path its owner (an `Output`'s staging
    /// path, or a path constructor's caller) chose; the caller names the path
    /// in an ordinary failure. A filesystem that refuses `O_DIRECT` at open
    /// fails with `ErrorKind::PlatformUnsupported` (feature `direct-io`).
    ///
    /// The target must be a regular file, or absent. Padding the tail and
    /// truncating to the logical size assume one, and a FIFO, socket or
    /// device is not a page-cache file `O_DIRECT` could bypass. An existing
    /// target that is not a regular file (symlinks followed, as the open
    /// follows them) is refused with
    /// [`OutputError::NotAFile`](crate::error::OutputError::NotAFile) and
    /// never opened: a FIFO without a reader would block the open, and
    /// Linux fails an `O_DIRECT` open of a FIFO or character device with
    /// `EINVAL`, which must not read as "direct I/O unavailable". Should the
    /// open still fail that way, the path is looked at again and a
    /// non-regular file found there is refused the same way. The opened
    /// descriptor is checked too, so a block device (whose `O_DIRECT` open
    /// succeeds) is never written, padded or truncated.
    ///
    /// That narrows, and does not close, the race with a process that
    /// replaces the target concurrently: a FIFO put in its place after the
    /// first look can still block the open, and a device put there sees the
    /// open. A path constructor's caller owns its destination; outputs that
    /// need more go through `Output`, whose staging file is created fresh.
    pub(crate) fn create_for_write(path: &Path) -> Result<(File, usize)> {
        refuse_non_regular(path)?;
        #[allow(clippy::disallowed_methods)]
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_DIRECT)
            .open(path);
        let file = match opened {
            Ok(file) => file,
            Err(e) => {
                if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) {
                    // Best effort: a failed look keeps the open's own error.
                    refuse_non_regular(path)?;
                }
                return Err(crate::error::direct_io_open_error(e));
            }
        };
        if !file.metadata()?.is_file() {
            return Err(not_a_file(path));
        }
        let align = resolve(query(&file, Access::Write)).map_err(|r| refusal(None, r))?;
        Ok((file, align))
    }

    /// Refuse `path` when it names an existing file that is not a regular
    /// file. Absent is fine (the open creates it); any other failure to look
    /// is returned as the I/O error it is.
    fn refuse_non_regular(path: &Path) -> Result<()> {
        #[allow(clippy::disallowed_methods)]
        match std::fs::metadata(path) {
            Ok(meta) if !meta.is_file() => Err(not_a_file(path)),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    #[cold]
    fn not_a_file(path: &Path) -> Error {
        crate::error::new_output_error(crate::error::OutputError::NotAFile {
            path: path.to_path_buf(),
        })
    }

    /// Open `path` for `O_DIRECT` reading, and return it with the alignment
    /// its reads must keep. The open `crate::read::input` has no flag for. A
    /// platform refusal names the path inside its source, as
    /// [`crate::error::direct_io_open_error_at`] renders it; any other
    /// failure is an ordinary I/O error the caller names.
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn open_for_read(path: &Path) -> Result<(File, usize)> {
        open_for_read_verified(path, path, 0, |_| Ok(()))
    }

    /// [`open_for_read`] of `open_path` with `extra_flags` added to
    /// `O_DIRECT`, for an input known to the user as `display`: `verify`
    /// runs on the opened file before its alignment is queried, so a file
    /// that is not the one expected is refused as that, never as the
    /// alignment refusal a different file could earn. Failures name
    /// `display`, never `open_path` (a pinned input reopens through
    /// `/proc/self/fd`).
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn open_for_read_verified(
        open_path: &Path,
        display: &Path,
        extra_flags: i32,
        verify: impl FnOnce(&File) -> Result<()>,
    ) -> Result<(File, usize)> {
        #[allow(clippy::disallowed_methods)]
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT | extra_flags)
            .open(open_path);
        let file = opened.map_err(|e| crate::error::direct_io_open_error_at(display, e))?;
        verify(&file)?;
        let align = resolve(query(&file, Access::Read)).map_err(|r| refusal(Some(display), r))?;
        Ok((file, align))
    }

    /// The alignment to use for what a file reported, or the report back
    /// when it cannot be kept. Each nonzero field must be a power of two;
    /// the maximum of powers of two then satisfies every one of them.
    pub(super) fn resolve(reported: Option<Reported>) -> std::result::Result<usize, Reported> {
        let Some(r) = reported else {
            return Ok(DIO_ALIGN_FLOOR);
        };
        let mut align = DIO_ALIGN_FLOOR;
        for field in [r.mem, r.offset] {
            if field == 0 {
                continue;
            }
            if !field.is_power_of_two() {
                return Err(r);
            }
            align = align.max(usize::try_from(field).map_err(|_| r)?);
        }
        if align > IO_BUF_BYTES {
            return Err(r);
        }
        Ok(align)
    }

    /// The refusal for an alignment [`resolve`] could not keep.
    #[cold]
    fn refusal(path: Option<&Path>, r: Reported) -> Error {
        let subject =
            path.map_or_else(|| "the output file".to_owned(), |p| p.display().to_string());
        Error::platform_unsupported(
            "direct-io",
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{subject} requires direct I/O alignment of {} bytes (memory) and {} \
                     bytes (offset), which the {IO_BUF_BYTES}-byte direct I/O buffers \
                     cannot keep",
                    r.mem, r.offset
                ),
            ),
        )
    }

    /// `STATX_DIO_READ_ALIGN` from the kernel UAPI; the `libc` crate defines
    /// the `stx_dio_read_offset_align` field but not the mask bit.
    #[cfg(all(target_env = "gnu", feature = "linux-direct-io"))]
    const STATX_DIO_READ_ALIGN: libc::c_uint = 0x0002_0000;

    /// Ask the kernel for `file`'s direct I/O alignment. `None` when nothing
    /// was reported, for any reason; see the module doc for why that falls
    /// back to the floor.
    #[cfg(target_env = "gnu")]
    fn query(file: &File, access: Access) -> Option<Reported> {
        use std::os::unix::io::AsRawFd;
        #[cfg(feature = "linux-direct-io")]
        let read_bit = if access == Access::Read {
            STATX_DIO_READ_ALIGN
        } else {
            0
        };
        #[cfg(not(feature = "linux-direct-io"))]
        let read_bit = {
            let _ = access;
            0
        };
        let mut stx = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // Safety: the fd is open for the call, the empty path with
        // AT_EMPTY_PATH names the fd itself, and `stx` is a writable statx.
        let rc = unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_DIOALIGN | read_bit,
                stx.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return None;
        }
        // Safety: zero-initialised, then filled by a successful statx.
        let stx = unsafe { stx.assume_init() };
        if stx.stx_mask & libc::STATX_DIOALIGN == 0 {
            return None;
        }
        // A read-specific offset alignment, when reported and nonzero,
        // replaces the general one for reads; zero means the general one
        // applies. Writers never use it.
        #[cfg(feature = "linux-direct-io")]
        let read_offset = if read_bit != 0 && stx.stx_mask & read_bit != 0 {
            stx.stx_dio_read_offset_align
        } else {
            0
        };
        #[cfg(not(feature = "linux-direct-io"))]
        let read_offset = 0;
        let offset = if read_offset != 0 {
            read_offset
        } else {
            stx.stx_dio_offset_align
        };
        Some(Reported {
            mem: stx.stx_dio_mem_align,
            offset,
        })
    }

    #[cfg(not(target_env = "gnu"))]
    fn query(_file: &File, _access: Access) -> Option<Reported> {
        None
    }

    /// Allocate `size` zeroed bytes aligned to `align`. Returns the pointer
    /// and the layout needed to deallocate it. The direct buffers
    /// (`DirectWriter`'s, the `io_uring` pool, `DirectReader`'s and the
    /// positional reader's scratch) come from here. A zero `size` is
    /// refused: a zero-sized allocation is not one the allocator may make.
    pub(crate) fn alloc_aligned_zeroed(
        size: usize,
        align: usize,
    ) -> io::Result<(std::ptr::NonNull<u8>, std::alloc::Layout)> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-sized aligned allocation",
            ));
        }
        let layout = std::alloc::Layout::from_size_align(size, align)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // Safety: layout has non-zero size (checked above).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        let ptr = std::ptr::NonNull::new(ptr)
            .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "aligned alloc failed"))?;
        Ok((ptr, layout))
    }

    /// Round `n` up to a multiple of `align`, a power of two.
    pub(crate) const fn round_up(n: usize, align: usize) -> usize {
        (n + align - 1) & !(align - 1)
    }
}

#[cfg(all(test, any(feature = "linux-direct-io", feature = "linux-io-uring")))]
mod tests {
    use super::DIO_ALIGN_FLOOR;
    use super::imp::{Reported, resolve, round_up};
    use crate::tunables::IO_BUF_BYTES;

    fn reported(mem: u32, offset: u32) -> Option<Reported> {
        Some(Reported { mem, offset })
    }

    #[test]
    fn nothing_reported_is_the_floor() {
        assert_eq!(resolve(None), Ok(DIO_ALIGN_FLOOR));
        assert_eq!(resolve(reported(0, 0)), Ok(DIO_ALIGN_FLOOR));
    }

    #[test]
    fn requirements_below_the_floor_keep_the_floor() {
        assert_eq!(resolve(reported(4, 512)), Ok(DIO_ALIGN_FLOOR));
        assert_eq!(resolve(reported(512, 4096)), Ok(DIO_ALIGN_FLOOR));
    }

    #[test]
    fn a_larger_requirement_is_used() {
        assert_eq!(resolve(reported(4, 16384)), Ok(16384));
        assert_eq!(resolve(reported(65536, 0)), Ok(65536));
        assert_eq!(resolve(reported(0, 8192)), Ok(8192));
    }

    #[test]
    fn a_requirement_the_buffers_cannot_keep_is_refused() {
        let oversize = u32::try_from(IO_BUF_BYTES * 2).unwrap_or(u32::MAX);
        assert!(resolve(reported(512, oversize)).is_err());
        // Not a power of two: an 8192-aligned address is not 6144-aligned.
        assert!(resolve(reported(6144, 8192)).is_err());
        assert!(resolve(reported(512, 3000)).is_err());
    }

    #[test]
    fn round_up_to_alignment() {
        assert_eq!(round_up(0, 4096), 0);
        assert_eq!(round_up(1, 4096), 4096);
        assert_eq!(round_up(4096, 4096), 4096);
        assert_eq!(round_up(4097, 4096), 8192);
        assert_eq!(round_up(4097, 16384), 16384);
    }
}
