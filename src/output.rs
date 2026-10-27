//! Output publication: the one policy behind every file or directory a
//! command produces.
//!
//! # The contract
//!
//! - **Nothing is written under the destination's name until the run has
//!   succeeded.** Constructing an output reserves a hidden staging sibling in
//!   the destination's directory (`.pbfhogg-tmp-<pid>-<n>.<name>`); the
//!   command writes there, and [`Output::publish`] renames it into place
//!   after the final flush and every post-write check. Dropping an output
//!   without publishing removes the staging entry, so a failed, cancelled,
//!   panicking or dropped run leaves nothing at the destination and leaves a
//!   pre-existing destination byte-identical. A run cancelled before
//!   [`Output::publish`] decides is refused there; once publication has
//!   decided it completes (ADR-0019). A process ended without unwinding
//!   (SIGKILL, the OOM killer, power loss, the CLI's second interrupt) skips
//!   the cleanup and can leave a hidden `.pbfhogg-tmp-*` sibling, never a
//!   file under the name the caller chose.
//! - **An existing destination is refused unless the caller asked to replace
//!   it** ([`Overwrite::Replace`]). The check runs at construction, which a
//!   caller does before handing the output to a command, so a refusal costs
//!   no work. Under [`Overwrite::Refuse`] publication itself is exclusive as
//!   well: a destination that appeared while the command ran is not
//!   replaced, and a filesystem that cannot publish exclusively is reported
//!   ([`OutputError::ExclusivePublishUnsupported`]) rather than raced.
//! - **A symlink is never an output path**, whatever the overwrite intent:
//!   renaming over it would silently turn the caller's link into a regular
//!   file. The error names the link and its target.
//! - **The destination must be of the right kind.** A file output refuses an
//!   existing directory or special file (renaming over `/dev/null` would
//!   replace the device node), a directory output refuses an existing
//!   non-directory or mount point, even under [`Overwrite::Replace`].
//! - **Nothing pbfhogg never deletes is deleted by publication.** A
//!   destination named like, or inside, a staging entry, a backup or a run
//!   directory is refused ([`OutputError::ReservedName`]). A directory to be
//!   replaced is walked first, and one that holds any of those names
//!   anywhere inside it is refused ([`OutputError::HoldsLeftovers`]); only an
//!   index's `.buckets-level*` scratch trees may go with it. The same walk
//!   runs before a replaced output's backup or a directory output's staging
//!   is removed recursively, and what it finds is kept and reported.
//! - **Only a finished writing session is published.** Every output is
//!   written through [`Output::writer`], [`Output::pbf_writer`] or
//!   [`OutputDir::writer`]; publication refuses an output nobody wrote, or
//!   whose writer was not finished or failed ([`OutputError::Unfinished`]).
//! - **Trouble that does not fail the run is reported, never dropped.** Every
//!   output is constructed with a [`Diagnostics`]; cleanup that fails, and
//!   anything kept back from cleanup, becomes an `output.cleanup` warning.
//!
//! The staging and backup names, and which an external tool may delete, are
//! defined in `reference/cli-reference.md`, "Names pbfhogg leaves behind".
//! Durability and timing are decided in ADR-0017: writers finish their files,
//! publication syncs them, and `PUBLISH_START`/`PUBLISH_END` bracket it.
//!
//! # Concurrency
//!
//! Under `Refuse` the guarantee holds against concurrent processes: the
//! final step is `renameat2(RENAME_NOREPLACE)` or, for files where that is
//! unavailable, `link(2)`, both of which fail rather than replace. Under
//! `Replace` the destination is re-inspected immediately before the rename;
//! another process mutating the destination entry in the instant between
//! that inspection and the rename is outside the contract (the rename never
//! follows a link, so nothing is written through one), and so is another
//! process creating entries inside a directory destination while it is being
//! published: the walks before a recursive removal narrow that window, they
//! do not close it. Nothing here makes a group of outputs visible to readers
//! atomically.
//!
//! # Shapes
//!
//! [`Output`] is one file, [`OutputDir`] one directory (the geocode index),
//! [`OutputSet`] a group of files published together (`extract --config`).
//! A group publishes all or nothing as far as failures allow: if a member
//! fails to install, members already installed are rolled back, and a
//! rollback step that cannot complete keeps the previous output in a
//! `.pbfhogg-backup-*` sibling and says so in the error.

// This module is the output choke point the crate-root deny points at: the
// staging, publication, rollback and directory syncs it owns are the calls
// `clippy.toml` refuses everywhere else.
#![allow(clippy::disallowed_methods)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::{Error, OutputError, Result, new_output_error};
use crate::host::Diagnostics;
use crate::write::metrics::WriterMetrics;

/// Whether an output may replace an existing destination.
///
/// There is no default: every caller states its intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    /// Refuse a destination that already exists, at construction and again,
    /// exclusively, at publication.
    Refuse,
    /// Replace an existing destination of the right kind.
    Replace,
}

impl Overwrite {
    /// `Replace` when `replace` is true, `Refuse` otherwise.
    #[must_use]
    pub const fn from_flag(replace: bool) -> Self {
        if replace { Self::Replace } else { Self::Refuse }
    }
}

/// An extra acceptance rule for an existing directory destination, run at
/// construction and again immediately before publication. The geocode
/// builder uses it to refuse replacing a directory that is not an index.
pub type Validator = fn(&Path) -> Result<()>;

/// A fault injected into publication by a test.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishFault {
    /// Fail after every recheck passed, before anything is installed: the
    /// latest point at which a run can fail with its output fully written.
    BeforeInstall,
    /// Fail after an existing destination was moved to its backup, before
    /// the new output is renamed in.
    AfterBackup,
    /// Force the `link(2)` fallback instead of `renameat2`.
    ForceLinkFallback,
    /// Fail after the `link(2)` that installs a file, before the staging
    /// name is unlinked.
    AfterLink,
    /// Fail after member `k` (0-based) of a group was installed.
    AfterInstalled(usize),
    /// Make every rollback restore step fail.
    Restore,
    /// Fail the directory sync after publication.
    DirSync,
    /// Fail the sync that makes a previous output's backup durable, after
    /// the output was moved aside and before the new one is installed.
    BackupSync,
    /// Fail the removal of a replaced output's backup after a successful
    /// publication.
    BackupRemoval,
}

#[cfg(feature = "test-hooks")]
type Faults = Vec<PublishFault>;
/// Without `test-hooks` there are no faults to carry.
#[cfg(not(feature = "test-hooks"))]
#[derive(Clone, Copy, Debug, Default)]
struct Faults;

#[cfg(feature = "test-hooks")]
fn fault(faults: &Faults, wanted: PublishFault) -> bool {
    faults.contains(&wanted)
}

/// Spell a fault in a way that compiles away without `test-hooks`.
macro_rules! fault_is {
    ($faults:expr, $variant:expr) => {{
        #[cfg(feature = "test-hooks")]
        let hit = fault($faults, $variant);
        #[cfg(not(feature = "test-hooks"))]
        let hit = {
            let _ = $faults;
            false
        };
        hit
    }};
}

fn injected(what: &str) -> Error {
    Error::internal(format!("injected publication fault: {what}"))
}

// ---------------------------------------------------------------------------
// The writing session
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Session {
    /// Nothing has started writing the staging file.
    Reserved,
    /// One writer owns the staging file and has not finished.
    Writing,
    /// The writer finished successfully.
    Finished,
    /// The writer failed, or was abandoned in a way that cannot be certified.
    /// Terminal.
    Failed,
}

/// The one writing session of an output, shared by the output and whatever
/// writes its staging file. Publication needs it `Finished`; an output
/// nobody opened a writer on, or whose writer is unfinished or failed, is
/// refused before anything is synced or installed. Nothing in the crate
/// publishes an output it did not write, so `Reserved` is not an exception.
///
/// Transitions are checked under a lock: `Reserved -> Writing -> Finished`,
/// and `Failed` is terminal, so a later success can never overwrite it.
///
/// It also carries the destination's path: the one owner of the output's
/// identity, so every writer filling it names the same file in an error.
#[derive(Clone, Debug)]
pub(crate) struct Completion(Arc<Mutex<Session>>, Arc<Path>);

impl Completion {
    fn new(destination: &Path) -> Self {
        Self(
            Arc::new(Mutex::new(Session::Reserved)),
            Arc::from(destination),
        )
    }

    /// The path the caller named for this output.
    pub(crate) fn destination(&self) -> &Arc<Path> {
        &self.1
    }

    fn state(&self) -> Session {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `Reserved -> Writing`; a second session on one output is an error.
    fn begin(&self, destination: &Path) -> Result<()> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *state == Session::Reserved {
            *state = Session::Writing;
            Ok(())
        } else {
            Err(Error::internal(format!(
                "output {} already has a writing session",
                destination.display()
            )))
        }
    }

    /// `Writing -> Finished`. Does nothing in any other state.
    pub(crate) fn finish(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *state == Session::Writing {
            *state = Session::Finished;
        }
    }

    /// Fail the session for good.
    pub(crate) fn fail(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Session::Failed;
    }
}

/// A callback run at the start of publication, before anything is synced or
/// installed. Exists so a front end can stop its own timer where the work
/// ends and publication begins.
struct Hook(Box<dyn FnOnce() + Send + Sync>);

impl std::fmt::Debug for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hook")
    }
}

/// Emits `PUBLISH_START` now and `PUBLISH_END` when dropped, so the pair
/// closes on every return. Just before `PUBLISH_END` it emits the
/// publication's own share of the writer-metrics sync counters,
/// `publish_sync_calls` and `publish_sync_ns`: deltas over this one span
/// (repeated publications emit repeated values), so a reader can tell the
/// syncs publication did from any a writer did. Markers and counters are
/// best effort: the sidecar drops one when its pipe is full.
struct PublishSpan<'a> {
    metrics: &'a WriterMetrics,
    sync_calls: u64,
    sync_ns: u64,
}

impl<'a> PublishSpan<'a> {
    fn start(metrics: &'a WriterMetrics) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let span = Self {
            metrics,
            sync_calls: metrics.sync_all_calls.load(Relaxed),
            sync_ns: metrics.sync_all_ns.load(Relaxed),
        };
        crate::debug::emit_marker("PUBLISH_START");
        span
    }
}

impl Drop for PublishSpan<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        // Both read before either is emitted.
        let calls = self
            .metrics
            .sync_all_calls
            .load(Relaxed)
            .saturating_sub(self.sync_calls);
        let ns = self
            .metrics
            .sync_all_ns
            .load(Relaxed)
            .saturating_sub(self.sync_ns);
        crate::debug::emit_counter("publish_sync_calls", calls);
        crate::debug::emit_counter("publish_sync_ns", ns);
        crate::debug::emit_marker("PUBLISH_END");
    }
}

/// A file output's buffered writer. The only way a command that is not a
/// `PbfWriter` (OSC, diff text, export) gets its output file: it holds the
/// output's single writing session, and [`finish`](Self::finish) is the one
/// place the buffer is flushed checked. Dropping it unfinished (an error path,
/// a panic) leaves the session open, so the output cannot be published, and
/// any failed `write` or `flush` fails the session for good: a caller that
/// carries on after a lost write cannot certify the output.
///
/// `finish` flushes; it does not sync. Durability belongs to publication,
/// which syncs every staged file after the run's timers have stopped.
#[derive(Debug)]
pub struct OutputWriter {
    inner: Option<io::BufWriter<CancellableFile>>,
    completion: Completion,
    cancel: crate::host::Invocation,
}

/// The staging file under an [`OutputWriter`]'s buffer: every write the
/// buffer passes down (one per buffer flush, not one per caller write) first
/// observes the invocation's cancellation token. A tripped token is
/// `ErrorKind::Cancelled` carried in an `io::Error` of kind `Other`, which no
/// `Write` layer retries (an `Interrupted` kind would be retried forever by
/// `write_all`, since the token stays tripped).
#[derive(Debug)]
struct CancellableFile {
    file: fs::File,
    cancel: crate::host::Invocation,
}

impl Write for CancellableFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.cancel.check().map_err(io::Error::from)?;
        self.file.write(buf)
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.cancel.check().map_err(io::Error::from)?;
        self.file.write_vectored(bufs)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl OutputWriter {
    /// Fail the session on any error but a retryable interruption, which
    /// wrote nothing, and name the output in it: a bare "No space left on
    /// device" does not say which of a run's files filled the disk. The
    /// error keeps its `io::ErrorKind`; the name rides inside it.
    fn checked<T>(&self, result: io::Result<T>) -> io::Result<T> {
        match result {
            Err(e) if e.kind() != io::ErrorKind::Interrupted => {
                self.completion.fail();
                Err(io::Error::from(
                    Error::from(e).writing(self.completion.destination()),
                ))
            }
            other => other,
        }
    }

    fn finished_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output writer already finished",
        )
    }
}

impl Write for OutputWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = match self.inner.as_mut() {
            Some(inner) => inner.write(buf),
            None => Err(Self::finished_error()),
        };
        self.checked(result)
    }

    /// Overridden so an error `write_all` makes up itself (a `WriteZero`)
    /// fails the session and names the output too, not only one `write`
    /// passes up.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let result = match self.inner.as_mut() {
            Some(inner) => inner.write_all(buf),
            None => Err(Self::finished_error()),
        };
        self.checked(result)
    }

    /// Overridden for the same reason as `write_all`: a `Display` that fails
    /// makes the default `write_fmt` invent an error no `write` returned.
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        let result = match self.inner.as_mut() {
            Some(inner) => inner.write_fmt(args),
            None => Err(Self::finished_error()),
        };
        self.checked(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = match self.inner.as_mut() {
            Some(inner) => inner.flush(),
            None => Ok(()),
        };
        self.checked(result)
    }
}

impl OutputWriter {
    /// Flush the buffer, checked (dropping a `BufWriter` swallows its last
    /// write error), and end the session. The file is released, never
    /// handed back, so nothing can be written after the output is certified.
    ///
    /// # Errors
    /// The final write failed, or the invocation was cancelled (checked here
    /// even when nothing is left in the buffer); the session is then failed
    /// for good.
    pub fn finish(mut self) -> Result<()> {
        let Some(inner) = self.inner.take() else {
            return Ok(());
        };
        if let Err(e) = self.cancel.check() {
            self.completion.fail();
            return Err(e);
        }
        match inner.into_inner() {
            Ok(file) => {
                drop(file);
                self.completion.finish();
                Ok(())
            }
            Err(e) => {
                self.completion.fail();
                Err(Error::from(e.into_error()).writing(self.completion.destination()))
            }
        }
    }
}

/// A directory output's writing session: the staging directory to fill, and
/// the declaration that it is complete. Publication refuses the output until
/// [`finish`](Self::finish) is called; dropping the writer unfinished (an
/// error path, a panic) leaves the output unpublishable.
///
/// This certifies the caller's declaration, not the files: whatever the
/// caller wrote must be flushed and closed before `finish`, and nothing may
/// write under [`path`](Self::path) afterwards. Publication syncs the tree.
#[derive(Debug)]
pub struct DirWriter {
    path: PathBuf,
    completion: Completion,
}

impl DirWriter {
    /// The staging directory to fill. It exists, empty, when the writer is
    /// opened.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Declare the directory complete.
    pub fn finish(self) {
        self.completion.finish();
    }

    /// Declare the directory failed: publication refuses it for good.
    pub fn fail(self) {
        self.completion.fail();
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// The longest tail of the destination name kept in a staging or backup
/// name. The prefix adds about 30 bytes, and common filesystems cap a name
/// at 255; keeping the tail keeps the destination's suffix.
const MAX_NAME_TAIL: usize = 200;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn bounded_tail(name: &OsStr) -> &OsStr {
    if let Some(text) = name.to_str() {
        if text.len() <= MAX_NAME_TAIL {
            return name;
        }
        // Cut on a character boundary, so a valid UTF-8 name stays valid
        // UTF-8 and suffix checks on the staging name keep working.
        let mut start = text.len() - MAX_NAME_TAIL;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        return OsStr::new(&text[start..]);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = name.as_bytes();
        if bytes.len() > MAX_NAME_TAIL {
            return OsStr::from_bytes(&bytes[bytes.len() - MAX_NAME_TAIL..]);
        }
    }
    name
}

/// A sibling of `dest` named `.pbfhogg-<kind>-<pid>-<n>.<name tail>`. The
/// marker is a prefix, so the sibling keeps the destination's suffix.
fn sibling(dest: &Path, kind: &str) -> Result<PathBuf> {
    let name = dest.file_name().ok_or_else(|| {
        new_output_error(OutputError::InvalidPath {
            path: dest.to_path_buf(),
        })
    })?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut sibling_name = OsString::from(format!(".pbfhogg-{kind}-{}-{seq}.", std::process::id()));
    sibling_name.push(bounded_tail(name));
    Ok(dest.with_file_name(sibling_name))
}

/// Create a fresh sibling of `dest` with `make`, retrying under a new name
/// while the name is taken. Exclusive by construction: `make` must fail with
/// `AlreadyExists` rather than reuse an entry.
fn reserve(dest: &Path, kind: &str, make: impl Fn(&Path) -> io::Result<()>) -> Result<PathBuf> {
    loop {
        let path = sibling(dest, kind)?;
        match make(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(Error::from(e).context(format!(
                    "cannot create a staging entry beside output {}",
                    dest.display()
                )));
            }
        }
    }
}

/// `dest` rebuilt from its components: a trailing `/` or `/.` is dropped,
/// so `link/` and `link/.` are inspected as `link` itself rather than
/// through it. A path ending in `..`, a root, or an empty path names
/// nothing a rename could publish.
fn normalize(dest: &Path) -> Result<PathBuf> {
    let name = dest.file_name().ok_or_else(|| {
        new_output_error(OutputError::InvalidPath {
            path: dest.to_path_buf(),
        })
    })?;
    Ok(match dest.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    })
}

fn parent_dir(dest: &Path) -> &Path {
    match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    File,
    Dir,
}

type Identity = (u64, u64);

#[cfg(unix)]
fn identity_of(meta: &fs::Metadata) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}
#[cfg(not(unix))]
fn identity_of(_meta: &fs::Metadata) -> Option<Identity> {
    None
}

/// Refuse a destination that is, or lies inside, one of the names pbfhogg
/// leaves behind. The path is checked as given and with its parent resolved,
/// so a symlinked parent cannot hide one.
fn refuse_reserved(dest: &Path) -> Result<()> {
    let mut spellings = vec![dest.to_path_buf()];
    if let Ok(parent) = fs::canonicalize(parent_dir(dest)) {
        spellings.push(parent);
    }
    for spelling in &spellings {
        for component in spelling.components() {
            if let std::path::Component::Normal(name) = component
                && crate::scratch::is_leftover_name(name)
            {
                return Err(new_output_error(OutputError::ReservedName {
                    path: dest.to_path_buf(),
                    component: PathBuf::from(name),
                }));
            }
        }
    }
    Ok(())
}

/// Refuse to replace a directory that holds anything pbfhogg never deletes.
fn refuse_leftovers_inside(dest: &Path) -> Result<()> {
    let entries = crate::scratch::protected_entries(dest).map_err(|e| {
        Error::from(e).context(format!(
            "cannot check what replacing output directory {} would delete",
            dest.display()
        ))
    })?;
    if entries.is_empty() {
        Ok(())
    } else {
        Err(new_output_error(OutputError::HoldsLeftovers {
            path: dest.to_path_buf(),
            entries,
        }))
    }
}

/// Apply the destination rules to whatever is at `dest` now.
fn inspect(
    dest: &Path,
    shape: Shape,
    overwrite: Overwrite,
    validator: Option<Validator>,
) -> Result<()> {
    refuse_reserved(dest)?;
    match fs::symlink_metadata(dest) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::from(e).context(format!("cannot inspect output {}", dest.display())));
        }
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(new_output_error(OutputError::Symlink {
                    path: dest.to_path_buf(),
                    target: fs::read_link(dest).unwrap_or_default(),
                }));
            }
            match shape {
                Shape::File if !meta.is_file() => {
                    return Err(new_output_error(OutputError::NotAFile {
                        path: dest.to_path_buf(),
                    }));
                }
                Shape::Dir if !meta.is_dir() => {
                    return Err(new_output_error(OutputError::NotADirectory {
                        path: dest.to_path_buf(),
                    }));
                }
                _ => {}
            }
            if overwrite == Overwrite::Refuse {
                return Err(new_output_error(OutputError::Exists {
                    path: dest.to_path_buf(),
                }));
            }
            // Publication renames the directory itself, which a mount point
            // cannot be; refuse before any work rather than at the end.
            if shape == Shape::Dir && crate::process::is_mount_point(dest)? {
                return Err(new_output_error(OutputError::MountPoint {
                    path: dest.to_path_buf(),
                }));
            }
            // Replacing a directory deletes the old one whole once the new
            // one is in place.
            if shape == Shape::Dir {
                refuse_leftovers_inside(dest)?;
            }
        }
    }
    if let Some(validator) = validator {
        validator(dest)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Exclusive rename
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = |path: &Path| {
        std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    };
    let from = c_path(from)?;
    let to = c_path(to)?;
    // The raw syscall rather than the libc wrapper: musl builds link against
    // C libraries that lack `renameat2`, and the kernel call is the same on
    // every Linux libc. A kernel without it answers ENOSYS, which the caller
    // treats as "no exclusive rename".
    // Safety: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// The kernel or filesystem has no exclusive rename. Anything else is a real
/// failure to report.
fn noreplace_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The filesystem cannot hard-link. `EPERM` is deliberately not on the
/// list: it also means a permission problem, which must surface as one.
///
/// This is not the same decision as the passthrough copy's fallback set
/// (`write::copy_range::kernel_copy_refused`), and the two lists are not
/// meant to match. A refused `link(2)` ends publication with a typed
/// refusal, so an errno on this list stops being reported as what it is;
/// a refused `copy_file_range` instead retries the same bytes through
/// `pread` + `write`, which surfaces any real permission or argument
/// problem on its own. That is why the copy set can take `EPERM` and
/// `EINVAL` and this one must not.
fn link_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EXDEV)
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// One destination
// ---------------------------------------------------------------------------

/// A previous destination moved aside under `Replace`: `entry` is the old
/// output, inside a `container` directory reserved exclusively for it, so
/// the move can never land on another run's backup.
#[derive(Debug)]
struct Backup {
    container: PathBuf,
    entry: PathBuf,
}

#[derive(Debug)]
enum State {
    /// Written (or being written) at the staging path; nothing at the
    /// destination is ours.
    Staged,
    /// The old destination was moved to `backup`; the new output is not
    /// installed, so the destination is (as far as we know) empty.
    BackedUp(Backup),
    /// The new output is at the destination.
    Installed {
        backup: Option<Backup>,
        identity: Option<Identity>,
    },
    /// Published for good. Backups are removable.
    Committed,
}

#[derive(Debug)]
struct Target {
    shape: Shape,
    dest: PathBuf,
    /// The reserved staging entry: the file itself, or a container directory
    /// for a directory output.
    staging: PathBuf,
    /// Where the command writes: `staging` for a file, `staging/new` for a
    /// directory.
    write: PathBuf,
    overwrite: Overwrite,
    validator: Option<Validator>,
    state: State,
    /// Published by `link(2)`: the staging name is a second link to the
    /// output until it is unlinked.
    linked: bool,
    /// The writing session; see [`Completion`].
    session: Completion,
    /// A file target's descriptor on the staging file, opened before anything
    /// writes it and kept until publication. Publication syncs through it:
    /// Linux reports a writeback error to the descriptions that were open when
    /// it happened, so a descriptor opened afterwards could miss one.
    handle: Option<fs::File>,
    /// A directory target's staging directory as reserved, so a staging root
    /// replaced (by a symlink, say) is refused rather than synced and
    /// installed.
    dir_identity: Option<Identity>,
    /// Run before anything is synced or installed.
    hooks: Vec<Hook>,
    /// Where cleanup trouble is reported. Required at construction, so it
    /// cannot be lost.
    diagnostics: Diagnostics,
    /// Cleanup trouble not yet reported. Emitted when the target drops,
    /// which is after every sync publication attempts: sink code never runs
    /// in the middle of making an output durable.
    pending: Vec<String>,
}

impl Target {
    fn prepare(
        dest: &Path,
        shape: Shape,
        overwrite: Overwrite,
        validator: Option<Validator>,
        diagnostics: &Diagnostics,
    ) -> Result<Self> {
        let dest = normalize(dest)?;
        inspect(&dest, shape, overwrite, validator)?;
        // A staging entry this constructor made and could not remove again
        // is reported, never left silently.
        let undo = |path: &Path, removed: io::Result<()>| {
            if let Err(e) = removed
                && e.kind() != io::ErrorKind::NotFound
            {
                emit_cleanup(
                    diagnostics,
                    &format!(
                        "could not remove the staging entry {} of output {} after its \
                         preparation failed: {e}; it can be deleted by hand",
                        path.display(),
                        dest.display()
                    ),
                );
            }
        };
        let (staging, write) = match shape {
            Shape::File => {
                let staging = reserve(&dest, "tmp", |path| {
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .map(drop)
                })?;
                (staging.clone(), staging)
            }
            Shape::Dir => {
                let staging = reserve(&dest, "tmp", |path| fs::create_dir(path))?;
                let write = staging.join("new");
                if let Err(e) = fs::create_dir(&write) {
                    undo(&staging, fs::remove_dir(&staging));
                    return Err(Error::from(e).context(format!(
                        "cannot create a staging directory for output {}",
                        dest.display()
                    )));
                }
                (staging, write)
            }
        };
        // Opened before any writer touches the file. The staging entry is
        // removed again if the open fails.
        let handle = match shape {
            Shape::File => match fs::OpenOptions::new().write(true).open(&write) {
                Ok(file) => Some(file),
                Err(e) => {
                    undo(&staging, fs::remove_file(&staging));
                    return Err(Error::from(e).context(format!(
                        "cannot open the staging file for output {}",
                        dest.display()
                    )));
                }
            },
            Shape::Dir => None,
        };
        let dir_identity = match shape {
            Shape::File => None,
            Shape::Dir => fs::symlink_metadata(&write)
                .ok()
                .and_then(|meta| identity_of(&meta)),
        };
        let session = Completion::new(&dest);
        Ok(Self {
            shape,
            dest,
            staging,
            write,
            overwrite,
            validator,
            state: State::Staged,
            linked: false,
            session,
            handle,
            dir_identity,
            hooks: Vec::new(),
            diagnostics: diagnostics.clone(),
            pending: Vec::new(),
        })
    }

    /// The session is finished, and the staging entry is still the file that
    /// was reserved (so what gets synced is what gets installed).
    fn check_ready(&self) -> Result<()> {
        if self.session.state() != Session::Finished {
            return Err(new_output_error(OutputError::Unfinished {
                path: self.dest.clone(),
            }));
        }
        let current = fs::symlink_metadata(&self.write).ok();
        let replaced = match self.shape {
            Shape::File => {
                let reserved = self
                    .handle
                    .as_ref()
                    .and_then(|handle| handle.metadata().ok())
                    .and_then(|meta| identity_of(&meta));
                reserved.is_none()
                    || reserved != current.as_ref().and_then(identity_of)
                    || !current.as_ref().is_some_and(fs::Metadata::is_file)
            }
            // Never followed through a symlink: what is synced and installed
            // must be the directory that was reserved.
            Shape::Dir => {
                !current.as_ref().is_some_and(fs::Metadata::is_dir)
                    || self.dir_identity != current.as_ref().and_then(identity_of)
            }
        };
        if replaced {
            return Err(new_output_error(OutputError::StagingReplaced {
                path: self.dest.clone(),
            }));
        }
        Ok(())
    }

    /// `fsync` the staged file or tree through the writer-metrics funnel.
    fn sync_staged(&self, metrics: &WriterMetrics) -> Result<()> {
        let result = match &self.handle {
            Some(handle) => metrics.sync_recorded(handle),
            None => sync_tree(&self.write, metrics),
        };
        result.map_err(|e| {
            Error::from(e).context(format!(
                "cannot sync the new contents of {} before publishing it",
                self.dest.display()
            ))
        })
    }

    fn recheck(&self) -> Result<()> {
        inspect(&self.dest, self.shape, self.overwrite, self.validator)
    }

    fn identity(&self) -> Option<Identity> {
        fs::symlink_metadata(&self.write)
            .ok()
            .and_then(|meta| identity_of(&meta))
    }

    /// Put the new output at the destination. `use_backup` moves an existing
    /// destination aside first (directories and group members, which may
    /// need restoring); a lone file under `Replace` is replaced in one
    /// atomic rename instead.
    fn install(
        &mut self,
        use_backup: bool,
        faults: &Faults,
        metrics: &WriterMetrics,
    ) -> Result<()> {
        let identity = self.identity();
        match self.overwrite {
            Overwrite::Refuse => self.install_exclusive(identity, faults),
            Overwrite::Replace => self.install_replacing(identity, use_backup, faults, metrics),
        }
    }

    fn install_exclusive(&mut self, identity: Option<Identity>, faults: &Faults) -> Result<()> {
        let attempt = if fault_is!(faults, PublishFault::ForceLinkFallback) {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        } else {
            rename_noreplace(&self.write, &self.dest)
        };
        match attempt {
            Ok(()) => {
                self.state = State::Installed {
                    backup: None,
                    identity,
                };
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(new_output_error(OutputError::Exists {
                    path: self.dest.clone(),
                }))
            }
            Err(e) if noreplace_unsupported(&e) => match self.shape {
                // No exclusive primitive for a directory: refuse rather than
                // let a plain rename replace an empty directory that is not
                // ours.
                Shape::Dir => Err(new_output_error(OutputError::ExclusivePublishUnsupported {
                    path: self.dest.clone(),
                    source: e,
                })),
                Shape::File => self.install_by_link(identity, faults),
            },
            Err(e) => {
                Err(Error::from(e).context(format!("cannot publish {}", self.dest.display())))
            }
        }
    }

    /// `link(2)` fails when the destination exists, so it is exclusive. It
    /// publishes at the moment it succeeds; removing the staging name after
    /// that is cleanup, never a publication failure.
    fn install_by_link(&mut self, identity: Option<Identity>, faults: &Faults) -> Result<()> {
        match fs::hard_link(&self.write, &self.dest) {
            Ok(()) => {
                self.state = State::Installed {
                    backup: None,
                    identity,
                };
                self.linked = true;
                if fault_is!(faults, PublishFault::AfterLink) {
                    return Err(injected("after link"));
                }
                drop(fs::remove_file(&self.write));
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(new_output_error(OutputError::Exists {
                    path: self.dest.clone(),
                }))
            }
            Err(e) if link_unsupported(&e) => {
                Err(new_output_error(OutputError::ExclusivePublishUnsupported {
                    path: self.dest.clone(),
                    source: e,
                }))
            }
            Err(e) => {
                Err(Error::from(e).context(format!("cannot publish {}", self.dest.display())))
            }
        }
    }

    fn install_replacing(
        &mut self,
        identity: Option<Identity>,
        use_backup: bool,
        faults: &Faults,
        metrics: &WriterMetrics,
    ) -> Result<()> {
        let present = match fs::symlink_metadata(&self.dest) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(Error::from(e)
                    .context(format!("cannot inspect output {}", self.dest.display())));
            }
        };
        let backup = if present && use_backup {
            let container = reserve(&self.dest, "backup", |path| fs::create_dir(path))?;
            let entry = container.join(self.dest.file_name().unwrap_or_default());
            if let Err(e) = fs::rename(&self.dest, &entry) {
                if let Err(e) = fs::remove_dir(&container) {
                    self.pending.push(format!(
                        "could not remove the empty backup directory {} after moving {} aside \
                         failed: {e}; it can be deleted by hand",
                        container.display(),
                        self.dest.display()
                    ));
                }
                return Err(Error::from(e).context(format!(
                    "cannot move existing output {} aside",
                    self.dest.display()
                )));
            }
            self.state = State::BackedUp(Backup { container, entry });
            if fault_is!(faults, PublishFault::AfterBackup) {
                return Err(injected("after backup"));
            }
            // The backup is the previous output's only name from here until
            // the new output is in place, so it is made durable before
            // anything relies on it: the entry inside its container, then
            // the container's link and the vacated name in the parent. A
            // failure is an install failure, and the rollback restores.
            if let State::BackedUp(backup) = &self.state {
                let synced = if fault_is!(faults, PublishFault::BackupSync) {
                    Err(io::Error::new(
                        io::ErrorKind::StorageFull,
                        "injected backup sync fault",
                    ))
                } else {
                    sync_dir(&backup.container, metrics)
                        .and_then(|()| sync_dir(parent_dir(&self.dest), metrics))
                };
                synced.map_err(|e| {
                    Error::from(e).context(format!(
                        "cannot make the backup of {} durable before replacing it",
                        self.dest.display()
                    ))
                })?;
            }
            match std::mem::replace(&mut self.state, State::Staged) {
                State::BackedUp(backup) => Some(backup),
                other => {
                    self.state = other;
                    return Err(Error::internal("output backup state lost"));
                }
            }
        } else {
            None
        };
        if let Err(e) = fs::rename(&self.write, &self.dest) {
            if let Some(backup) = backup {
                self.state = State::BackedUp(backup);
            }
            return Err(Error::from(e).context(format!("cannot publish {}", self.dest.display())));
        }
        self.state = State::Installed { backup, identity };
        Ok(())
    }

    /// Undo this member's installation. `Some` describes what could not be
    /// undone; whatever it names is left in place, never deleted.
    fn rollback(&mut self, faults: &Faults) -> Option<String> {
        let fail_restore = fault_is!(faults, PublishFault::Restore);
        match std::mem::replace(&mut self.state, State::Staged) {
            State::Staged => None,
            State::Committed => {
                self.state = State::Committed;
                None
            }
            State::BackedUp(backup) => {
                let dest_free = matches!(
                    fs::symlink_metadata(&self.dest),
                    Err(ref e) if e.kind() == io::ErrorKind::NotFound
                );
                if !fail_restore && dest_free && fs::rename(&backup.entry, &self.dest).is_ok() {
                    self.remove_empty_backup(&backup);
                    None
                } else {
                    let note = format!(
                        "{}: the previous output could not be restored and is kept at {}",
                        self.dest.display(),
                        backup.entry.display()
                    );
                    self.state = State::BackedUp(backup);
                    Some(note)
                }
            }
            State::Installed { backup, identity } => {
                let ours = match fs::symlink_metadata(&self.dest) {
                    Ok(meta) => {
                        !meta.file_type().is_symlink()
                            && (identity.is_none() || identity_of(&meta) == identity)
                    }
                    Err(_) => false,
                };
                let restored = !fail_restore && ours && self.uninstall(backup.as_ref());
                if restored {
                    if let Some(backup) = backup {
                        self.remove_empty_backup(&backup);
                    }
                    None
                } else {
                    let note = match &backup {
                        Some(backup) => format!(
                            "{}: the new output could not be rolled back; the previous output \
                             is kept at {}",
                            self.dest.display(),
                            backup.entry.display()
                        ),
                        None => format!(
                            "{}: the new output could not be removed and remains in place",
                            self.dest.display()
                        ),
                    };
                    self.state = State::Installed { backup, identity };
                    Some(note)
                }
            }
        }
    }

    /// Take our output back out of the destination and put the backup (if
    /// any) back. The new output returns to the staging area, where Drop
    /// removes it.
    fn uninstall(&self, backup: Option<&Backup>) -> bool {
        let moved_back = match self.shape {
            Shape::Dir => fs::rename(&self.dest, &self.write).is_ok(),
            Shape::File => match backup {
                // The restore below replaces our file in one rename.
                Some(_) => true,
                None => fs::remove_file(&self.dest).is_ok(),
            },
        };
        if !moved_back {
            return false;
        }
        match backup {
            Some(backup) => fs::rename(&backup.entry, &self.dest).is_ok(),
            None => true,
        }
    }

    /// Remove a backup container that a restore just emptied.
    fn remove_empty_backup(&mut self, backup: &Backup) {
        if let Err(e) = fs::remove_dir(&backup.container) {
            self.pending.push(format!(
                "{} was restored, but removing its emptied backup directory {} failed: {e}; it \
                 can be deleted by hand",
                self.dest.display(),
                backup.container.display()
            ));
        }
    }

    /// Mark published and drop what publication no longer needs.
    ///
    /// With `remove_backup` false (the parent sync failed, so the
    /// replacement is not known to be durable) a backup is kept and its
    /// container returned. With it true the backup is removed, unless the
    /// walk before the recursive removal finds something pbfhogg never
    /// deletes, or cannot complete: then it is kept and reported. Cleanup is
    /// best effort: the output is already in place, so trouble is recorded
    /// for the warning, never returned as an error.
    fn commit(&mut self, remove_backup: bool, faults: &Faults) -> Option<PathBuf> {
        let mut kept = None;
        if let State::Installed {
            backup: Some(backup),
            ..
        } = std::mem::replace(&mut self.state, State::Committed)
        {
            if remove_backup {
                self.remove_backup(&backup, faults);
            } else {
                kept = Some(backup.container);
            }
        }
        if self.shape == Shape::Dir
            && let Err(e) = fs::remove_dir(&self.staging)
        {
            self.pending.push(format!(
                "{} was published, but removing its staging directory {} failed: {e}; it can \
                 be deleted by hand",
                self.dest.display(),
                self.staging.display()
            ));
        }
        kept
    }

    fn remove_backup(&mut self, backup: &Backup, faults: &Faults) {
        match crate::scratch::protected_entries(&backup.container) {
            Ok(found) if found.is_empty() => {}
            Ok(found) => {
                let found: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
                self.pending.push(format!(
                    "{} was published; the backup of the previous output at {} is kept because \
                     it holds {}, which pbfhogg never deletes. Check it, then remove it yourself",
                    self.dest.display(),
                    backup.container.display(),
                    found.join(", ")
                ));
                return;
            }
            Err(e) => {
                self.pending.push(format!(
                    "{} was published; the backup of the previous output at {} is kept because \
                     what it holds could not be checked ({e}). Check it, then remove it yourself",
                    self.dest.display(),
                    backup.container.display()
                ));
                return;
            }
        }
        let removed = if fault_is!(faults, PublishFault::BackupRemoval) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected backup removal fault",
            ))
        } else {
            fs::remove_dir_all(&backup.container)
        };
        if let Err(e) = removed {
            self.pending.push(format!(
                "{} was published, but removing the backup of the previous output at {} \
                 failed: {e}; the cleanup is incomplete and what is left there can be \
                 deleted by hand",
                self.dest.display(),
                backup.container.display()
            ));
        }
    }

    /// Remove an unpublished directory output's staging container, unless
    /// something pbfhogg never deletes ended up inside it (or the check
    /// cannot complete): then it is kept and reported.
    fn drop_dir_staging(&mut self) {
        match crate::scratch::protected_entries(&self.staging) {
            Ok(found) if found.is_empty() => {}
            Ok(found) => {
                let found: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
                self.pending.push(format!(
                    "{} was not published; its staging directory {} is kept because it holds {}, \
                     which pbfhogg never deletes",
                    self.dest.display(),
                    self.staging.display(),
                    found.join(", ")
                ));
                return;
            }
            Err(e) => {
                self.pending.push(format!(
                    "{} was not published; its staging directory {} is kept because what it \
                     holds could not be checked ({e})",
                    self.dest.display(),
                    self.staging.display()
                ));
                return;
            }
        }
        let staging = self.staging.clone();
        self.remove_or_note(fs::remove_dir_all(&staging), &staging);
    }

    fn remove_or_note(&mut self, removed: io::Result<()>, path: &Path) {
        if let Err(e) = removed
            && e.kind() != io::ErrorKind::NotFound
        {
            self.pending.push(format!(
                "could not remove the staging entry {} of output {}: {e}; it can be deleted by \
                 hand",
                path.display(),
                self.dest.display()
            ));
        }
    }
}

/// Report cleanup trouble. A sink that panicked while a panic is already
/// unwinding would abort the process, so nothing is emitted then: a
/// panicking run's leftovers are found by the next scan instead.
fn emit_cleanup(diagnostics: &Diagnostics, message: &str) {
    if std::thread::panicking() {
        return;
    }
    diagnostics.emit(
        crate::host::EventKind::Warning,
        "output.cleanup",
        format_args!("{message}"),
    );
}

impl Drop for Target {
    fn drop(&mut self) {
        match self.state {
            // Only ever the staging entry this run reserved. A backup is a
            // separate `.pbfhogg-backup-*` sibling and is never touched here:
            // after a failed restore it may be the only copy of the old
            // output.
            //
            // A directory's staging container holds this run's new
            // generation (or nothing, once it was moved into place); it is
            // removed whole in every state but `Committed`, after the same
            // check that guards every recursive removal.
            State::Staged | State::BackedUp(_) | State::Installed { .. } => match self.shape {
                Shape::Dir => self.drop_dir_staging(),
                // After a plain rename the staging name is gone (the removal
                // finds nothing, which is not reported); it is still there
                // after a link install (removing that name leaves the
                // published output intact) and after a failed install.
                Shape::File => {
                    let write = self.write.clone();
                    self.remove_or_note(fs::remove_file(&write), &write);
                }
            },
            // A link-installed file whose staging name could not be unlinked
            // at publication gets one more try here.
            State::Committed => {
                if self.linked {
                    let write = self.write.clone();
                    self.remove_or_note(fs::remove_file(&write), &write);
                }
            }
        }
        for message in std::mem::take(&mut self.pending) {
            emit_cleanup(&self.diagnostics, &message);
        }
    }
}

/// Install every member, rolling back on the first failure, then commit and
/// sync. The one publication routine behind every shape. Every sync records
/// into `metrics`, the publishing invocation's writer set.
fn publish_all(targets: &mut [Target], faults: &Faults, metrics: &WriterMetrics) -> Result<()> {
    // Every member is ready before anything runs, so a refusal leaves the
    // hooks unrun and every destination untouched.
    for target in targets.iter() {
        target.check_ready()?;
    }
    // The caller's timers stop here: the work is done, and what follows is
    // durability and publication.
    for target in targets.iter_mut() {
        for Hook(hook) in std::mem::take(&mut target.hooks) {
            hook();
        }
    }
    let _span = PublishSpan::start(metrics);
    for target in targets.iter() {
        // Again after the hooks: a hook is caller code and may have failed
        // a session (a write after the end of stream, say). A failure on
        // another thread after this point wrote nothing (a finished writer
        // holds no file), so it cannot change what is installed.
        target.check_ready()?;
        target.recheck()?;
    }
    // Make every new generation durable before it replaces anything, so a
    // delayed write error stops the publication instead of surfacing after
    // the previous output is gone. This is the only place an output file is
    // synced: writers only finish their files.
    for target in targets.iter() {
        target.sync_staged(metrics)?;
    }
    if fault_is!(faults, PublishFault::BeforeInstall) {
        return Err(injected("before install"));
    }
    let group = targets.len() > 1;
    for i in 0..targets.len() {
        let use_backup = group || targets[i].shape == Shape::Dir;
        let mut result = targets[i].install(use_backup, faults, metrics);
        if result.is_ok() && fault_is!(faults, PublishFault::AfterInstalled(i)) {
            result = Err(injected("after a member was installed"));
        }
        if let Err(error) = result {
            let touched = &mut targets[..=i];
            let unresolved: Vec<String> = touched
                .iter_mut()
                .rev()
                .filter_map(|target| target.rollback(faults))
                .collect();
            // Best effort: the restored names are visible now; syncing their
            // directories (and any backup kept by a failed restore) is all
            // that can be done for their durability on an error path.
            for parent in distinct_parents(touched) {
                drop(sync_dir(&parent, metrics));
            }
            for target in touched.iter() {
                if let State::BackedUp(backup)
                | State::Installed {
                    backup: Some(backup),
                    ..
                } = &target.state
                {
                    drop(sync_dir(&backup.container, metrics));
                }
            }
            if unresolved.is_empty() {
                return Err(error);
            }
            return Err(new_output_error(OutputError::RollbackIncomplete {
                unresolved,
                source: error,
            }));
        }
    }
    // The renames are done and stay done; a sync failure only means their
    // durability is unconfirmed, and says so. Every parent is attempted, and
    // the first failure is the one reported. Backups are removed only once
    // every parent is durable: until then a backup may be the only copy that
    // is, so on a failure every one is kept and named.
    let mut unsynced: Option<(PathBuf, io::Error)> = None;
    for parent in distinct_parents(targets) {
        let result = if fault_is!(faults, PublishFault::DirSync) {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected directory sync fault",
            ))
        } else {
            sync_dir(&parent, metrics)
        };
        if let Err(source) = result
            && unsynced.is_none()
        {
            let dest = targets
                .iter()
                .find(|target| parent_dir(&target.dest) == parent)
                .map_or_else(|| parent.clone(), |target| target.dest.clone());
            unsynced = Some((dest, source));
        }
    }
    let remove_backups = unsynced.is_none();
    let mut kept = Vec::new();
    for target in targets.iter_mut() {
        kept.extend(target.commit(remove_backups, faults));
    }
    let Some((path, source)) = unsynced else {
        return Ok(());
    };
    let mut kept_synced = true;
    for container in &kept {
        kept_synced &= sync_dir(container, metrics).is_ok();
    }
    // Cleanup warnings are emitted when the targets drop, after all of this.
    Err(new_output_error(OutputError::Unsynced {
        path,
        source,
        kept,
        kept_synced,
    }))
}

/// The parent directories of `targets`, each once, in order.
fn distinct_parents(targets: &[Target]) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = Vec::new();
    for target in targets {
        let parent = parent_dir(&target.dest).to_path_buf();
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    parents
}

#[cfg(unix)]
fn sync_dir(dir: &Path, metrics: &WriterMetrics) -> io::Result<()> {
    metrics.sync_recorded(&fs::File::open(dir)?)
}

/// Sync every regular file under `dir`, then every directory, the root
/// last.
fn sync_tree(dir: &Path, metrics: &WriterMetrics) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path(), metrics)?;
        } else if kind.is_file() {
            metrics.sync_recorded(&fs::File::open(entry.path())?)?;
        }
    }
    sync_dir(dir, metrics)
}
#[cfg(not(unix))]
fn sync_dir(_dir: &Path, _metrics: &WriterMetrics) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Public shapes
// ---------------------------------------------------------------------------

/// One output file.
///
/// Construct it before any work: [`Output::new`] applies the destination
/// rules and reserves the staging file. Write it through exactly one writing
/// session - [`Output::pbf_writer`] for a PBF, [`Output::writer`] for
/// anything else - finish that writer, then [`Output::publish`]. Library
/// command entry points take an `Output` by value and publish it themselves
/// once they have succeeded.
#[derive(Debug)]
pub struct Output {
    target: Target,
    #[cfg(feature = "test-hooks")]
    faults: Faults,
}

impl Output {
    /// Check `destination` against the output rules and reserve its staging
    /// file. `diagnostics` receives the `output.cleanup` warnings: cleanup
    /// that failed, and anything kept back from cleanup.
    ///
    /// # Errors
    /// [`OutputError::Exists`] for an existing destination under
    /// [`Overwrite::Refuse`]; [`OutputError::Symlink`] for a symlink;
    /// [`OutputError::NotAFile`] for a directory or special file;
    /// [`OutputError::InvalidPath`] for a path that names no file;
    /// [`OutputError::ReservedName`] for a path named like, or inside, an
    /// entry pbfhogg leaves behind; an I/O error if the staging file cannot
    /// be created (the directory is missing or not writable).
    pub fn new(
        destination: impl AsRef<Path>,
        overwrite: Overwrite,
        diagnostics: &Diagnostics,
    ) -> Result<Self> {
        Ok(Self {
            target: Target::prepare(
                destination.as_ref(),
                Shape::File,
                overwrite,
                None,
                diagnostics,
            )?,
            #[cfg(feature = "test-hooks")]
            faults: Faults::new(),
        })
    }

    /// The path the caller named, for messages and identity checks. Never
    /// write here.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.target.dest
    }

    /// The staging file, for reading back what was written (its size, say).
    /// It exists (empty) from construction until publication or drop. Write
    /// it only through [`writer`](Self::writer) or
    /// [`pbf_writer`](Self::pbf_writer): publication refuses an output no
    /// writing session finished, and bytes written past a session are not
    /// part of what it certified.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.target.write
    }

    /// Open the output's single writing session as a pipelined [`PbfWriter`]
    /// on `backend`, with `header_block_bytes` (a serialized `HeaderBlock`,
    /// as [`HeaderBuilder::build`](crate::block_builder::HeaderBuilder::build)
    /// makes) already written. The session finishes when the writer's
    /// [`flush`](PbfWriter::flush) succeeds; any error the writer reports
    /// fails it for good. The writer does not `fsync` at its end of stream:
    /// [`publish`](Self::publish) does.
    ///
    /// # Errors
    /// The output already has a writing session; `backend` needs a feature
    /// that is not compiled in; the staging file cannot be opened or the
    /// header written. The session is failed in each case.
    ///
    /// The writer observes `host.invocation`: once it is tripped every write
    /// method returns `ErrorKind::Cancelled` (failing the session), and the
    /// writer thread stops a queued `copy_file_range` splice between kernel
    /// calls.
    pub fn pbf_writer(
        &self,
        compression: crate::write::compression::Compression,
        header_block_bytes: &[u8],
        backend: crate::write::OutputBackend,
        host: &crate::host::Host,
    ) -> Result<crate::write::writer::PbfWriter<crate::write::file_writer::FileWriter>> {
        use crate::write::OutputBackend;
        use crate::write::writer::PbfWriter;
        let completion = self.begin_write()?;
        let path = self.path();
        let finish = crate::write::Finish::Deferred;
        // A test can make every backend refuse as a platform would; the
        // refusal is the constructor's result, so the failure handling below
        // runs exactly as for a real one.
        #[cfg(feature = "test-hooks")]
        let refused = host.invocation.faults().output_backend_refusal();
        #[cfg(not(feature = "test-hooks"))]
        let refused: Option<Error> = None;
        let writer = if let Some(refusal) = refused {
            Err(refusal)
        } else {
            match backend {
                OutputBackend::Uring => {
                    #[cfg(feature = "linux-io-uring")]
                    {
                        PbfWriter::to_path_uring_in(
                            path,
                            compression,
                            header_block_bytes,
                            host.executor.clone(),
                            host.diagnostics.clone(),
                            finish,
                            host.invocation.clone(),
                        )
                    }
                    #[cfg(not(feature = "linux-io-uring"))]
                    {
                        Err(Error::invalid_argument(
                            "the io_uring output backend needs the linux-io-uring feature, \
                         which this build was compiled without",
                        ))
                    }
                }
                OutputBackend::Direct => {
                    #[cfg(feature = "linux-direct-io")]
                    {
                        PbfWriter::to_path_direct_in(
                            path,
                            compression,
                            header_block_bytes,
                            host.executor.clone(),
                            host.diagnostics.clone(),
                            finish,
                            host.invocation.clone(),
                        )
                    }
                    #[cfg(not(feature = "linux-direct-io"))]
                    {
                        Err(Error::invalid_argument(
                            "the O_DIRECT output backend needs the linux-direct-io feature, \
                         which this build was compiled without",
                        ))
                    }
                }
                OutputBackend::Parallel => PbfWriter::to_path_parallel_in(
                    path,
                    compression,
                    header_block_bytes,
                    host.executor.clone(),
                    host.diagnostics.clone(),
                    finish,
                    host.invocation.clone(),
                ),
                OutputBackend::Buffered => PbfWriter::to_path_in(
                    path,
                    compression,
                    header_block_bytes,
                    host.executor.clone(),
                    host.diagnostics.clone(),
                    finish,
                    host.invocation.clone(),
                ),
            }
        };
        // A constructor failure (the staging file, the writer thread's
        // start, the header write) names the output like every later one.
        let writer = writer.map_err(|e| {
            completion.fail();
            e.writing(completion.destination())
        })?;
        Ok(writer.with_completion(completion))
    }

    /// The overwrite intent this output was built with.
    #[must_use]
    pub fn overwrite(&self) -> Overwrite {
        self.target.overwrite
    }

    /// Open the output's single writing session for a writer that is not a
    /// `PbfWriter`: the staging file, truncated, behind an [`OutputWriter`]
    /// whose [`finish`](OutputWriter::finish) is the one checked flush.
    /// [`publish`](Self::publish) refuses an output whose writer was never
    /// finished.
    ///
    /// The writer observes `cancel` (the invocation's token) at every buffer
    /// it passes to the file and in `finish`; a tripped token fails the
    /// session with `ErrorKind::Cancelled`.
    ///
    /// # Errors
    /// The output already has a writing session, or the staging file cannot
    /// be opened (the error names the destination).
    pub fn writer(&self, cancel: &crate::host::Invocation) -> Result<OutputWriter> {
        let completion = self.begin_write()?;
        match fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.target.write)
        {
            Ok(file) => Ok(OutputWriter {
                inner: Some(io::BufWriter::new(CancellableFile {
                    file,
                    cancel: cancel.clone(),
                })),
                completion,
                cancel: cancel.clone(),
            }),
            Err(e) => {
                completion.fail();
                Err(Error::from(e).context(format!(
                    "cannot open the staging file for output {}",
                    self.target.dest.display()
                )))
            }
        }
    }

    /// Start the output's writing session for a writer that opens
    /// [`path`](Self::path) itself (the PBF backends). The returned lease is
    /// finished by that writer's successful end of stream; until then, and
    /// after any failure, publication refuses the output.
    pub(crate) fn begin_write(&self) -> Result<Completion> {
        self.target.session.begin(&self.target.dest)?;
        Ok(self.target.session.clone())
    }

    /// Run `hook` when publication starts, before anything is synced or
    /// installed (and before the `PUBLISH_START` marker). A front end uses
    /// it to stop a wall-clock timer where the work ends: the sync and the
    /// rename that follow are not part of the work being timed. Hooks run in
    /// the order added; every member of a group runs its hooks before any of
    /// them syncs. A hook must not fail; a panic unwinds before any
    /// destination was touched.
    #[must_use]
    pub fn before_publish(mut self, hook: impl FnOnce() + Send + Sync + 'static) -> Self {
        self.target.hooks.push(Hook(Box::new(hook)));
        self
    }

    /// Sync the staging file and rename it into place. Call only after the
    /// writer has been finished and every check on the output has passed; an
    /// output no writing session finished (none was opened, or it is open or
    /// failed) is refused ([`OutputError::Unfinished`]) before anything is
    /// synced or installed.
    ///
    /// `cancel` is the invocation's token, and checking it is the publication
    /// decision, the first thing publication does: a token tripped by then
    /// refuses the output (`ErrorKind::Cancelled`; the output drops and its
    /// staging entry is removed, nothing is installed). Once the decision is
    /// taken publication runs to completion whatever happens to the token,
    /// and its result - success, [`OutputError::Unsynced`], a refusal - is
    /// authoritative. Publication observes only the token passed here; the
    /// command entry points pass the token their work ran under.
    ///
    /// # Errors
    /// The destination rules are re-applied first; under
    /// [`Overwrite::Refuse`] a destination that appeared meanwhile is
    /// refused exclusively. [`OutputError::Unsynced`] means the output *was*
    /// published and only the directory sync failed.
    pub fn publish(mut self, cancel: &crate::host::Invocation) -> Result<()> {
        cancel.check()?;
        let faults = self.faults();
        publish_all(
            std::slice::from_mut(&mut self.target),
            &faults,
            &cancel.metrics().writer,
        )
    }

    #[cfg(feature = "test-hooks")]
    fn faults(&self) -> Faults {
        self.faults.clone()
    }
    #[cfg(not(feature = "test-hooks"))]
    fn faults(&self) -> Faults {
        Faults
    }

    /// Test hook: inject a publication fault.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_fault(&mut self, fault: PublishFault) {
        self.faults.push(fault);
    }
}

/// One output directory, written in a staging directory and swapped into
/// place as a whole.
#[derive(Debug)]
pub struct OutputDir {
    target: Target,
    #[cfg(feature = "test-hooks")]
    faults: Faults,
}

impl OutputDir {
    /// Check `destination` against the output rules (plus `validator`, if
    /// given) and reserve the staging directory.
    ///
    /// # Errors
    /// As [`Output::new`], with [`OutputError::NotADirectory`] and
    /// [`OutputError::MountPoint`] in place of `NotAFile`;
    /// [`OutputError::HoldsLeftovers`] for an existing directory to be
    /// replaced that holds anything pbfhogg never deletes; plus whatever
    /// `validator` refuses.
    pub fn new(
        destination: impl AsRef<Path>,
        overwrite: Overwrite,
        validator: Option<Validator>,
        diagnostics: &Diagnostics,
    ) -> Result<Self> {
        Ok(Self {
            target: Target::prepare(
                destination.as_ref(),
                Shape::Dir,
                overwrite,
                validator,
                diagnostics,
            )?,
            #[cfg(feature = "test-hooks")]
            faults: Faults::new(),
        })
    }

    /// The directory the caller named. Never write here.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.target.dest
    }

    /// Open the output's single writing session: the staging directory to
    /// fill, and the [`DirWriter::finish`] that lets it be published.
    ///
    /// # Errors
    /// The output already has a writing session.
    pub fn writer(&self) -> Result<DirWriter> {
        self.target.session.begin(&self.target.dest)?;
        Ok(DirWriter {
            path: self.target.write.clone(),
            completion: self.target.session.clone(),
        })
    }

    /// Swap the staging directory into place. An existing destination is
    /// moved to a backup first (made durable before the swap) and restored
    /// if the swap fails. Refused before anything is synced or installed
    /// unless a [`DirWriter`] was finished.
    ///
    /// `cancel` is checked once, as the publication decision, exactly as in
    /// [`Output::publish`].
    ///
    /// # Errors
    /// As [`Output::publish`]; [`OutputError::RollbackIncomplete`] if the
    /// swap failed and the previous directory could not be put back (the
    /// error names where it is kept).
    pub fn publish(mut self, cancel: &crate::host::Invocation) -> Result<()> {
        cancel.check()?;
        #[cfg(feature = "test-hooks")]
        let faults = self.faults.clone();
        #[cfg(not(feature = "test-hooks"))]
        let faults = Faults;
        publish_all(
            std::slice::from_mut(&mut self.target),
            &faults,
            &cancel.metrics().writer,
        )
    }

    /// Test hook: inject a publication fault.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_fault(&mut self, fault: PublishFault) {
        self.faults.push(fault);
    }
}

/// A group of output files published together.
///
/// Every member is checked at construction, so a refusal for the last
/// member costs no work, and two members resolving to one file are refused.
/// Members are lent out as `&Output` only; the set publishes them all at
/// once.
#[derive(Debug)]
pub struct OutputSet {
    members: Vec<Output>,
    #[cfg(feature = "test-hooks")]
    faults: Faults,
}

impl OutputSet {
    /// Check every destination and reserve a staging file for each, every
    /// member reporting to `diagnostics`.
    ///
    /// # Errors
    /// Any member's [`Output::new`] error, or
    /// [`OutputError::Collision`] when two destinations name one file (the
    /// same canonical directory and name, or an existing hard link).
    pub fn files<I, P>(
        destinations: I,
        overwrite: Overwrite,
        diagnostics: &Diagnostics,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let paths = destinations
            .into_iter()
            .map(|path| normalize(path.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let mut planned: Vec<(PathBuf, Option<Identity>)> = Vec::with_capacity(paths.len());
        for path in &paths {
            let parent = fs::canonicalize(parent_dir(path)).map_err(|e| {
                Error::from(e).context(format!(
                    "cannot resolve the directory of output {}",
                    path.display()
                ))
            })?;
            let resolved = parent.join(path.file_name().unwrap_or_default());
            let identity = fs::metadata(path).ok().and_then(|meta| identity_of(&meta));
            if let Some(index) = planned.iter().position(|(other, other_identity)| {
                *other == resolved || (identity.is_some() && *other_identity == identity)
            }) {
                return Err(new_output_error(OutputError::Collision {
                    first: paths[index].clone(),
                    second: path.clone(),
                }));
            }
            planned.push((resolved, identity));
        }
        let members = paths
            .iter()
            .map(|path| Output::new(path, overwrite, diagnostics))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            members,
            #[cfg(feature = "test-hooks")]
            faults: Faults::new(),
        })
    }

    /// Number of members.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the set has no members.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Member `index`, borrowed.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Output> {
        self.members.get(index)
    }

    /// The members in order, borrowed.
    pub fn iter(&self) -> impl Iterator<Item = &Output> {
        self.members.iter()
    }

    /// Publish every member: recheck all, install each, and on a failure
    /// roll back the members already installed.
    ///
    /// `cancel` is checked once for the whole group, as its publication
    /// decision, before any member's hook runs or anything is installed;
    /// after it the group publishes (or rolls back) to completion.
    ///
    /// # Errors
    /// As [`Output::publish`], plus [`OutputError::RollbackIncomplete`]
    /// naming every member whose previous state could not be restored.
    pub fn publish(mut self, cancel: &crate::host::Invocation) -> Result<()> {
        cancel.check()?;
        #[cfg(feature = "test-hooks")]
        let faults = self.faults.clone();
        #[cfg(not(feature = "test-hooks"))]
        let faults = Faults;
        let mut targets: Vec<Target> = Vec::with_capacity(self.members.len());
        for member in self.members.drain(..) {
            targets.push(member.into_target());
        }
        publish_all(&mut targets, &faults, &cancel.metrics().writer)
    }

    /// Test hook: inject a publication fault.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_fault(&mut self, fault: PublishFault) {
        self.faults.push(fault);
    }
}

impl Output {
    fn into_target(self) -> Target {
        self.target
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        crate::path_guard::tests::scratch()
    }

    /// Every entry in `dir` except the ones named.
    fn others(dir: &Path, keep: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|name| !keep.contains(&name.as_str()))
            .collect();
        names.sort();
        names
    }

    fn output_error(err: &Error) -> &OutputError {
        match err.kind() {
            crate::ErrorKind::Output(output) => output,
            other => panic!("expected an output error, got {other:?}: {err}"),
        }
    }

    fn discard() -> Diagnostics {
        Diagnostics::discard()
    }

    /// Write `bytes` through the output's writing session and finish it.
    fn stage(out: &Output, bytes: &[u8]) -> Result<()> {
        let mut writer = out.writer(&crate::host::Invocation::new())?;
        writer.write_all(bytes)?;
        writer.finish()
    }

    /// Write one file into a directory output's session and finish it.
    fn stage_dir(out: &OutputDir, name: &str, bytes: &[u8]) {
        let writer = out.writer().unwrap();
        fs::write(writer.path().join(name), bytes).unwrap();
        writer.finish();
    }

    #[test]
    fn a_fresh_output_publishes_and_leaves_nothing_else() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        assert!(!dest.exists(), "nothing at the destination before publish");
        stage(&out, b"new").unwrap();
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
    }

    #[test]
    fn an_existing_output_is_refused_at_construction() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        fs::write(&dest, b"user data").unwrap();
        let err = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Exists { path } if *path == dest));
        // The text names no front-end flag.
        assert!(!err.to_string().contains("--"), "{err}");
        assert_eq!(fs::read(&dest).unwrap(), b"user data");
        assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
    }

    #[test]
    fn replace_overwrites_an_existing_output() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        fs::write(&dest, b"old").unwrap();
        let out = Output::new(&dest, Overwrite::Replace, &discard()).unwrap();
        stage(&out, b"new").unwrap();
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
    }

    /// The failure path: a run that drops its output without publishing
    /// leaves a pre-existing destination byte-identical and no staging file.
    #[test]
    fn a_dropped_output_publishes_nothing() {
        let dir = scratch();
        let dest = dir.path().join("important.json");
        fs::write(&dest, b"user data").unwrap();
        {
            let out = Output::new(&dest, Overwrite::Replace, &discard()).unwrap();
            stage(&out, b"complete-looking output").unwrap();
        }
        assert_eq!(fs::read(&dest).unwrap(), b"user data");
        assert!(others(dir.path(), &["important.json"]).is_empty());

        let fresh = dir.path().join("fresh.json");
        {
            let out = Output::new(&fresh, Overwrite::Refuse, &discard()).unwrap();
            stage(&out, b"complete-looking output").unwrap();
        }
        assert!(!fresh.exists());
        assert!(others(dir.path(), &["important.json"]).is_empty());
    }

    /// Under `Refuse`, a destination that appears while the command runs is
    /// not replaced at publication.
    #[test]
    fn refuse_is_exclusive_at_publication() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        stage(&out, b"ours").unwrap();
        fs::write(&dest, b"theirs").unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Exists { .. }));
        assert_eq!(fs::read(&dest).unwrap(), b"theirs");
        assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_in_every_spelling_and_under_replace() {
        let dir = scratch();
        let target = dir.path().join("target.osm.pbf");
        fs::write(&target, b"user data").unwrap();
        let link = dir.path().join("link.osm.pbf");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dangling = dir.path().join("dangling.osm.pbf");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
        let link_text = link.to_str().unwrap();
        for spelling in [
            link.clone(),
            PathBuf::from(format!("{link_text}/")),
            PathBuf::from(format!("{link_text}/.")),
            dangling.clone(),
        ] {
            for overwrite in [Overwrite::Refuse, Overwrite::Replace] {
                let err = Output::new(&spelling, overwrite, &discard()).unwrap_err();
                match output_error(&err) {
                    OutputError::Symlink { path, target: to } => {
                        assert!(path == &link || path == &dangling, "{err}");
                        assert!(!to.as_os_str().is_empty(), "the target is named: {err}");
                    }
                    other => panic!("{spelling:?}: expected a symlink refusal, got {other:?}"),
                }
                assert!(err.to_string().contains(" is a symlink to "), "{err}");
            }
        }
        assert_eq!(fs::read(&target).unwrap(), b"user data");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A directory output too.
        let dir_target = dir.path().join("real-dir");
        fs::create_dir(&dir_target).unwrap();
        let dir_link = dir.path().join("dir-link");
        std::os::unix::fs::symlink(&dir_target, &dir_link).unwrap();
        let err = OutputDir::new(&dir_link, Overwrite::Replace, None, &discard()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Symlink { .. }));
    }

    #[test]
    fn the_wrong_kind_is_refused_even_under_replace() {
        let dir = scratch();
        let a_dir = dir.path().join("a-dir");
        fs::create_dir(&a_dir).unwrap();
        let err = Output::new(&a_dir, Overwrite::Replace, &discard()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::NotAFile { .. }));

        let a_file = dir.path().join("a-file");
        fs::write(&a_file, b"x").unwrap();
        let err = OutputDir::new(&a_file, Overwrite::Replace, None, &discard()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::NotADirectory { .. }
        ));

        #[cfg(unix)]
        {
            let err = Output::new("/dev/null", Overwrite::Replace, &discard()).unwrap_err();
            assert!(matches!(output_error(&err), OutputError::NotAFile { .. }));
        }
    }

    #[test]
    fn a_path_naming_nothing_is_refused() {
        for path in ["..", "/", "a/.."] {
            let err = Output::new(path, Overwrite::Replace, &discard()).unwrap_err();
            assert!(
                matches!(output_error(&err), OutputError::InvalidPath { .. }),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn a_missing_directory_is_reported_before_any_work() {
        let dir = scratch();
        let dest = dir.path().join("no-such-dir").join("out.osm.pbf");
        let err = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap_err();
        assert!(err.report().contains("out.osm.pbf"), "{}", err.report());
    }

    /// Writers that pick an encoding from the name's tail (merge-changes
    /// gzips a `.gz`) see the same tail on the staging file, and a long
    /// UTF-8 name stays valid UTF-8 when shortened.
    #[test]
    fn the_staging_name_keeps_the_suffix() {
        let dir = scratch();
        let dest = dir.path().join("changes.osc.gz");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let name = out.path().file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(".pbfhogg-tmp-") && name.ends_with(".osc.gz"),
            "{name}"
        );
        assert_eq!(out.path().parent(), dest.parent());
        drop(out);

        let long = format!("{}.osc.gz", "\u{e9}".repeat(120));
        let dest = dir.path().join(&long);
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let name = out.path().file_name().unwrap();
        let name = name
            .to_str()
            .expect("a UTF-8 destination keeps a UTF-8 staging name");
        assert!(name.ends_with(".osc.gz") && name.len() <= 255, "{name}");
    }

    #[test]
    fn a_directory_output_swaps_in_whole() {
        let dir = scratch();
        let dest = dir.path().join("index");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("old.bin"), b"old").unwrap();
        let out = OutputDir::new(&dest, Overwrite::Replace, None, &discard()).unwrap();
        stage_dir(&out, "new.bin", b"new");
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert!(!dest.join("old.bin").exists());
        assert_eq!(fs::read(dest.join("new.bin")).unwrap(), b"new");
        assert!(others(dir.path(), &["index"]).is_empty());

        let err = OutputDir::new(&dest, Overwrite::Refuse, None, &discard()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Exists { .. }));
    }

    #[test]
    fn a_directory_validator_runs_at_construction_and_publication() {
        fn refuse_marked(path: &Path) -> Result<()> {
            if path.join("marker").exists() {
                Err(Error::invalid_argument("marked"))
            } else {
                Ok(())
            }
        }
        let dir = scratch();
        let dest = dir.path().join("index");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("marker"), b"").unwrap();
        assert!(
            OutputDir::new(&dest, Overwrite::Replace, Some(refuse_marked), &discard()).is_err()
        );
        fs::remove_file(dest.join("marker")).unwrap();
        let out =
            OutputDir::new(&dest, Overwrite::Replace, Some(refuse_marked), &discard()).unwrap();
        stage_dir(&out, "new.bin", b"new");
        fs::write(dest.join("marker"), b"").unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(
            err.to_string().contains("marked"),
            "the validator is rerun before the swap: {err}"
        );
        assert!(
            dest.join("marker").exists(),
            "a refused swap touches nothing"
        );
        assert!(others(dir.path(), &["index"]).is_empty());
    }

    #[test]
    fn a_set_refuses_colliding_members_and_existing_ones_up_front() {
        let dir = scratch();
        let a = dir.path().join("a.osm.pbf");
        let b = dir.path().join("b.osm.pbf");
        let err = OutputSet::files(
            [&a, &dir.path().join(".").join("a.osm.pbf")],
            Overwrite::Refuse,
            &discard(),
        )
        .unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Collision { .. }));
        fs::write(&b, b"user data").unwrap();
        let err = OutputSet::files([&a, &b], Overwrite::Refuse, &discard()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Exists { .. }));
        assert!(!a.exists());
        assert!(others(dir.path(), &["b.osm.pbf"]).is_empty());
    }

    /// A diagnostics handle that records `(key, text)` of every event.
    #[derive(Default)]
    struct Recording(Mutex<Vec<(&'static str, String)>>);
    impl crate::host::DiagnosticSink for Recording {
        fn event(&self, event: &crate::host::Event<'_>) {
            self.0
                .lock()
                .unwrap()
                .push((event.key, event.args.to_string()));
        }
    }
    fn recording() -> (Arc<Recording>, Diagnostics) {
        let sink = Arc::new(Recording::default());
        let diagnostics =
            Diagnostics::new(Arc::clone(&sink) as Arc<dyn crate::host::DiagnosticSink>);
        (sink, diagnostics)
    }

    #[test]
    fn a_finished_writer_publishes_and_an_unfinished_one_does_not() {
        let dir = scratch();
        let dest = dir.path().join("out.osc");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let mut writer = out.writer(&crate::host::Invocation::new()).unwrap();
        writer.write_all(b"<osmChange/>").unwrap();
        // Bytes are still in the buffer: the output is not publishable.
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        assert!(!dest.exists());
        drop(writer);
        assert!(others(dir.path(), &[]).is_empty(), "staging is removed");

        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let mut writer = out.writer(&crate::host::Invocation::new()).unwrap();
        writer.write_all(b"<osmChange/>").unwrap();
        writer.finish().unwrap();
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"<osmChange/>");
    }

    #[test]
    fn an_output_has_one_writing_session() {
        let dir = scratch();
        let out = Output::new(dir.path().join("o"), Overwrite::Refuse, &discard()).unwrap();
        let first = out.writer(&crate::host::Invocation::new()).unwrap();
        assert!(
            out.writer(&crate::host::Invocation::new()).is_err(),
            "a second writer would truncate the first"
        );
        assert!(out.begin_write().is_err());
        drop(first);
    }

    /// No session, no publication: writing through `path` behind the
    /// session's back cannot be certified, and an empty output is a writer
    /// finished with nothing written.
    #[test]
    fn an_output_nobody_wrote_is_refused() {
        let dir = scratch();
        let dest = dir.path().join("empty");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        fs::write(out.path(), b"behind the session's back").unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        assert!(!dest.exists());
        assert!(others(dir.path(), &[]).is_empty());

        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        stage(&out, b"").unwrap();
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"");

        let index = dir.path().join("index");
        let out = OutputDir::new(&index, Overwrite::Refuse, None, &discard()).unwrap();
        let writer = out.writer().unwrap();
        fs::write(writer.path().join("f"), b"x").unwrap();
        drop(writer);
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        assert!(!index.exists());
        let out = OutputDir::new(&index, Overwrite::Refuse, None, &discard()).unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        assert!(others(dir.path(), &["empty"]).is_empty());
    }

    /// A failed write fails the session for good: the caller cannot carry
    /// on past a lost write and certify the output with a later finish.
    #[test]
    fn a_failed_write_fails_the_session() {
        let dir = scratch();
        let host = crate::host::Host::for_tests();
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let out = Output::new(
            dir.path().join("out.osm.pbf"),
            Overwrite::Refuse,
            &discard(),
        )
        .unwrap();
        let mut writer = out
            .pbf_writer(
                crate::write::compression::Compression::None,
                &header,
                crate::write::OutputBackend::Buffered,
                &host,
            )
            .unwrap();
        // A raw frame that is not a frame is still bytes to the writer; a
        // write after the end of stream is the error every backend reports.
        writer.flush().unwrap();
        assert!(writer.write_raw(b"late").is_err());
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));

        // The synchronous path: a framing error before anything was written.
        let out = Output::new(
            dir.path().join("sync.osm.pbf"),
            Overwrite::Refuse,
            &discard(),
        )
        .unwrap();
        let completion = out.begin_write().unwrap();
        let mut writer = crate::write::writer::PbfWriter::to_staged_file(
            out.path(),
            Default::default(),
            &crate::host::Invocation::new(),
        )
        .unwrap()
        .with_completion(completion);
        assert!(writer.write_primitive_block(b"\xff\xff\xff").is_err());
        writer.flush().unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
    }

    #[test]
    fn a_reserved_name_is_refused_as_or_above_the_destination() {
        let dir = scratch();
        for name in [
            ".pbfhogg-tmp-1-0.out.pbf",
            ".pbfhogg-backup-1-0.out.pbf",
            ".pbfhogg-run-1-ab",
            ".geocode-build-3",
            "derive-creates-1.xml.tmp",
        ] {
            let err =
                Output::new(dir.path().join(name), Overwrite::Replace, &discard()).unwrap_err();
            assert!(
                matches!(output_error(&err), OutputError::ReservedName { .. }),
                "{name}: {err}"
            );
        }
        let backup = dir.path().join(".pbfhogg-backup-1-0.out.pbf");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("out.pbf"), b"only copy").unwrap();
        let err = Output::new(backup.join("out.pbf"), Overwrite::Replace, &discard()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::ReservedName { .. }
        ));
        let err =
            OutputDir::new(backup.join("index"), Overwrite::Replace, None, &discard()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::ReservedName { .. }
        ));
        // Through a symlinked parent too.
        let link = dir.path().join("plain");
        std::os::unix::fs::symlink(&backup, &link).unwrap();
        let err = Output::new(link.join("out.pbf"), Overwrite::Replace, &discard()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::ReservedName { .. }
        ));
        assert_eq!(fs::read(backup.join("out.pbf")).unwrap(), b"only copy");
    }

    /// A directory to be replaced is refused, before any work and again
    /// before the swap, while it holds anything pbfhogg never deletes.
    #[test]
    fn a_directory_holding_leftovers_is_not_replaced() {
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch();
        let index = dir.path().join("index");
        fs::create_dir_all(index.join("sub")).unwrap();
        fs::write(index.join("header"), b"index").unwrap();
        // Index scratch may go with it.
        fs::create_dir_all(index.join(".buckets-level17").join(".pbfhogg-run-1-ab")).unwrap();
        let out = OutputDir::new(&index, Overwrite::Replace, None, &discard()).unwrap();
        drop(out);
        // A backup deep inside, with a name that is not UTF-8.
        let mut name = b".pbfhogg-backup-1-0.".to_vec();
        name.push(0xff);
        let backup = index.join("sub").join(std::ffi::OsStr::from_bytes(&name));
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("previous"), b"only copy").unwrap();
        let err = OutputDir::new(&index, Overwrite::Replace, None, &discard()).unwrap_err();
        let OutputError::HoldsLeftovers { entries, .. } = output_error(&err) else {
            panic!("expected a leftover refusal, got {err}");
        };
        assert_eq!(entries, std::slice::from_ref(&backup));
        fs::remove_dir_all(&backup).unwrap();

        // Planted after construction: refused at publication, untouched.
        let out = OutputDir::new(&index, Overwrite::Replace, None, &discard()).unwrap();
        stage_dir(&out, "header", b"new");
        let run = index.join(".pbfhogg-run-1-cd");
        fs::create_dir(&run).unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::HoldsLeftovers { .. }
        ));
        assert!(run.exists());
        assert_eq!(fs::read(index.join("header")).unwrap(), b"index");
        assert!(others(dir.path(), &["index"]).is_empty());
    }

    /// Dropping a directory output never deletes something pbfhogg never
    /// deletes that ended up in its staging: it keeps the staging and warns.
    #[test]
    fn dropped_directory_staging_keeps_protected_entries() {
        let dir = scratch();
        let (sink, diagnostics) = recording();
        let out = OutputDir::new(
            dir.path().join("index"),
            Overwrite::Refuse,
            None,
            &diagnostics,
        )
        .unwrap();
        let writer = out.writer().unwrap();
        let inner = writer.path().join(".pbfhogg-backup-1-0.x");
        fs::create_dir(&inner).unwrap();
        drop(writer);
        drop(out);
        assert!(inner.exists());
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "output.cleanup");
        assert!(events[0].1.contains("is kept"), "{events:?}");
    }

    /// A staging entry that cannot be removed is reported, not lost.
    #[test]
    fn a_staging_file_that_cannot_be_removed_is_reported() {
        // Root ignores directory permissions, so the removal cannot fail there.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = scratch();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let (sink, diagnostics) = recording();
        let out = Output::new(sub.join("out"), Overwrite::Refuse, &diagnostics).unwrap();
        let staging = out.path().to_path_buf();
        fs::set_permissions(&sub, std::os::unix::fs::PermissionsExt::from_mode(0o500)).unwrap();
        drop(out);
        fs::set_permissions(&sub, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            events[0].1.contains(&staging.display().to_string()),
            "{events:?}"
        );
    }

    /// A PBF writer's lease only finishes on a successful flush: a writer
    /// that was written to and never flushed cannot be published past.
    #[test]
    fn a_pbf_writer_that_was_not_flushed_blocks_publication() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        let host = crate::host::Host::for_tests();
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();

        // A writer was started and has not reached a successful flush (a
        // `PbfWriter` dropped unflushed would trip its own test assertion, so
        // the bare lease stands in for it).
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let lease = out.begin_write().unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        drop(lease);
        assert!(!dest.exists());

        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let mut writer = crate::commands::writer_from_header_bytes(
            &out,
            crate::write::compression::Compression::None,
            &header,
            crate::write::OutputBackend::Buffered,
            &host,
        )
        .unwrap();
        writer.flush().unwrap();
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert!(dest.exists());
        // Finishing is terminal: a flush that already failed is never
        // certified by a second one.
        let out = Output::new(
            dir.path().join("second.osm.pbf"),
            Overwrite::Refuse,
            &discard(),
        )
        .unwrap();
        let mut writer = crate::commands::writer_from_header_bytes(
            &out,
            crate::write::compression::Compression::None,
            &header,
            crate::write::OutputBackend::Buffered,
            &host,
        )
        .unwrap();
        out.target.session.fail();
        writer.flush().unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
    }

    #[test]
    fn a_replaced_staging_file_is_refused() {
        let dir = scratch();
        let dest = dir.path().join("out");
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        stage(&out, b"ours").unwrap();
        let staging = out.path().to_path_buf();
        fs::remove_file(&staging).unwrap();
        fs::write(&staging, b"someone else").unwrap();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::StagingReplaced { .. }
        ));
        assert!(!dest.exists());
    }

    /// A directory output's staging root swapped for a symlink is refused,
    /// not followed, synced and installed as a link.
    #[test]
    fn a_replaced_staging_directory_is_refused() {
        let dir = scratch();
        let dest = dir.path().join("index");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let out = OutputDir::new(&dest, Overwrite::Refuse, None, &discard()).unwrap();
        let writer = out.writer().unwrap();
        fs::remove_dir(writer.path()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, writer.path()).unwrap();
        writer.finish();
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::StagingReplaced { .. }
        ));
        assert!(fs::symlink_metadata(&dest).is_err());
        assert!(elsewhere.is_dir());
    }

    /// A hook that fails a session (here: a write after the end of stream)
    /// stops the publication, though the session was finished when it began.
    #[test]
    fn a_session_failed_by_a_hook_is_not_published() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        let host = crate::host::Host::for_tests();
        let header = crate::block_builder::HeaderBuilder::new().build().unwrap();
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let mut writer = out
            .pbf_writer(
                crate::write::compression::Compression::None,
                &header,
                crate::write::OutputBackend::Buffered,
                &host,
            )
            .unwrap();
        writer.flush().unwrap();
        let writer = Mutex::new(writer);
        let out = out.before_publish(move || {
            assert!(writer.lock().unwrap().write_raw(b"late").is_err());
        });
        let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Unfinished { .. }));
        assert!(!dest.exists());
    }

    /// The hook runs when publication starts: nothing is installed yet and
    /// the staged bytes are all there. It is the point a timer stops at.
    #[test]
    fn before_publish_hooks_run_before_anything_is_installed() {
        let dir = scratch();
        let dest = dir.path().join("out");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
        let mut writer = out.writer(&crate::host::Invocation::new()).unwrap();
        writer.write_all(b"data").unwrap();
        writer.finish().unwrap();
        let (probe_dest, probe_staging, log) =
            (dest.clone(), out.path().to_path_buf(), Arc::clone(&seen));
        let out = out.before_publish(move || {
            log.lock().unwrap().push((
                probe_dest.exists(),
                fs::read(&probe_staging).unwrap_or_default(),
            ));
        });
        out.publish(&crate::host::Invocation::new()).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![(false, b"data".to_vec())]);
        assert_eq!(fs::read(&dest).unwrap(), b"data");
    }

    /// A refused publication runs no hooks: the timer is not stopped for a
    /// publication that never starts.
    #[test]
    fn a_refused_publication_runs_no_hooks() {
        let dir = scratch();
        let out = Output::new(dir.path().join("out"), Overwrite::Refuse, &discard()).unwrap();
        let _writer = out.writer(&crate::host::Invocation::new()).unwrap();
        let ran = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&ran);
        let out = out.before_publish(move || *flag.lock().unwrap() = true);
        assert!(out.publish(&crate::host::Invocation::new()).is_err());
        assert!(!*ran.lock().unwrap());
    }

    /// Counted on the run's own invocation, so exact: publication syncs the
    /// staged file and its directory (Unix only: elsewhere `sync_dir` is a
    /// no-op).
    #[cfg(unix)]
    #[test]
    fn publication_syncs_through_the_writer_metrics_funnel() {
        use std::sync::atomic::Ordering::Relaxed;
        let dir = scratch();
        let token = crate::host::Invocation::new();
        let out = Output::new(dir.path().join("out"), Overwrite::Refuse, &discard()).unwrap();
        let mut writer = out.writer(&token).unwrap();
        writer.write_all(b"data").unwrap();
        writer.finish().unwrap();
        out.publish(&token).unwrap();
        assert_eq!(token.metrics().writer.sync_all_calls.load(Relaxed), 2);
    }

    /// The walk before a backup's recursive removal: something pbfhogg
    /// never deletes that appeared in the old output after the last check
    /// keeps the backup whole, and the warning says why.
    #[test]
    fn a_backup_holding_protected_entries_is_kept() {
        let dir = scratch();
        let (sink, diagnostics) = recording();
        let mut target = Target::prepare(
            &dir.path().join("index"),
            Shape::Dir,
            Overwrite::Refuse,
            None,
            &diagnostics,
        )
        .unwrap();
        let container = dir.path().join(".pbfhogg-backup-1-0.index");
        let entry = container.join("index");
        let run = entry.join(".pbfhogg-run-1-ab");
        fs::create_dir_all(&run).unwrap();
        fs::write(entry.join("old-file"), b"old").unwrap();
        let backup = Backup { container, entry };
        #[cfg(feature = "test-hooks")]
        let faults = Faults::new();
        #[cfg(not(feature = "test-hooks"))]
        let faults = Faults;
        target.remove_backup(&backup, &faults);
        drop(target);
        assert!(run.exists());
        assert_eq!(fs::read(backup.entry.join("old-file")).unwrap(), b"old");
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].1.contains("is kept"), "{events:?}");
        assert!(
            events[0].1.contains(&run.display().to_string()),
            "{events:?}"
        );
    }

    #[cfg(feature = "test-hooks")]
    mod faults {
        use super::*;

        /// A replaced directory whose backup cannot be removed afterwards:
        /// the replacement still succeeds, and the warning names the backup.
        #[test]
        fn a_backup_that_cannot_be_removed_is_reported_not_fatal() {
            let dir = scratch();
            let dest = dir.path().join("index");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("old-file"), b"old").unwrap();
            let (sink, diagnostics) = recording();
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None, &diagnostics).unwrap();
            stage_dir(&out, "new-file", b"new");
            out.inject_fault(PublishFault::BackupRemoval);
            out.publish(&crate::host::Invocation::new()).unwrap();
            assert_eq!(fs::read(dest.join("new-file")).unwrap(), b"new");
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1, "{events:?}");
            assert_eq!(events[0].0, "output.cleanup");
            assert!(events[0].1.contains("was published"), "{events:?}");
            let left = others(dir.path(), &["index"]);
            assert_eq!(left.len(), 1, "{left:?}");
            assert!(left[0].starts_with(".pbfhogg-backup-"), "{left:?}");
            assert!(events[0].1.contains(left[0].as_str()), "{events:?}");
            assert_eq!(
                fs::read(dir.path().join(&left[0]).join("index").join("old-file")).unwrap(),
                b"old",
                "the previous output is untouched"
            );
        }

        /// A failing parent sync means the replacement is not known to be
        /// durable, so the backup is not removed at all: it is kept, named in
        /// the error, and nothing is reported as a cleanup failure.
        #[test]
        fn an_unsynced_publication_keeps_and_names_the_backup() {
            let dir = scratch();
            let dest = dir.path().join("index");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("old-file"), b"old").unwrap();
            let (sink, diagnostics) = recording();
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None, &diagnostics).unwrap();
            stage_dir(&out, "new-file", b"new");
            out.inject_fault(PublishFault::DirSync);
            let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
            let OutputError::Unsynced {
                kept, kept_synced, ..
            } = output_error(&err)
            else {
                panic!("expected an unsynced publication, got {err}");
            };
            assert!(kept_synced);
            let left = others(dir.path(), &["index"]);
            assert_eq!(left.len(), 1, "{left:?}");
            assert!(left[0].starts_with(".pbfhogg-backup-"), "{left:?}");
            assert_eq!(kept, &[dir.path().join(&left[0])]);
            assert!(err.to_string().contains(left[0].as_str()), "{err}");
            assert_eq!(
                fs::read(dir.path().join(&left[0]).join("index").join("old-file")).unwrap(),
                b"old",
                "the previous output is kept whole"
            );
            assert_eq!(fs::read(dest.join("new-file")).unwrap(), b"new");
            assert!(sink.0.lock().unwrap().is_empty());
        }

        /// A backup whose sync fails before the swap is an install failure:
        /// the previous output is put back and nothing new is installed.
        #[test]
        fn a_backup_that_cannot_be_made_durable_is_restored() {
            let dir = scratch();
            let dest = dir.path().join("index");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("old-file"), b"old").unwrap();
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None, &discard()).unwrap();
            stage_dir(&out, "new-file", b"new");
            out.inject_fault(PublishFault::BackupSync);
            assert!(out.publish(&crate::host::Invocation::new()).is_err());
            assert_eq!(fs::read(dest.join("old-file")).unwrap(), b"old");
            assert!(!dest.join("new-file").exists());
            assert!(others(dir.path(), &["index"]).is_empty());
        }

        fn staged_set(dir: &Path, overwrite: Overwrite) -> (OutputSet, PathBuf, PathBuf) {
            let a = dir.join("a.osm.pbf");
            let b = dir.join("b.osm.pbf");
            let set = OutputSet::files([&a, &b], overwrite, &discard()).unwrap();
            for member in set.iter() {
                stage(member, b"new").unwrap();
            }
            (set, a, b)
        }

        /// The latest failure a run can have - everything written, nothing
        /// installed - publishes nothing.
        #[test]
        fn a_failure_before_install_publishes_nothing() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            fs::write(&dest, b"user data").unwrap();
            let mut out = Output::new(&dest, Overwrite::Replace, &discard()).unwrap();
            stage(&out, b"new").unwrap();
            out.inject_fault(PublishFault::BeforeInstall);
            assert!(out.publish(&crate::host::Invocation::new()).is_err());
            assert_eq!(fs::read(&dest).unwrap(), b"user data");
            assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
        }

        #[test]
        fn the_link_fallback_publishes_exclusively() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            let mut out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
            stage(&out, b"new").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            out.publish(&crate::host::Invocation::new()).unwrap();
            assert_eq!(fs::read(&dest).unwrap(), b"new");
            assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());

            // Exclusive: an entry that appeared meanwhile is not replaced.
            let other = dir.path().join("other.osm.pbf");
            let mut out = Output::new(&other, Overwrite::Refuse, &discard()).unwrap();
            stage(&out, b"ours").unwrap();
            fs::write(&other, b"theirs").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
            assert!(matches!(output_error(&err), OutputError::Exists { .. }));
            assert_eq!(fs::read(&other).unwrap(), b"theirs");
        }

        /// A failure after the link published the file rolls it back: the
        /// link committed it, so the rollback must remove it.
        #[test]
        fn a_failure_after_link_rolls_the_output_back() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            let mut out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
            stage(&out, b"new").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            out.inject_fault(PublishFault::AfterLink);
            assert!(out.publish(&crate::host::Invocation::new()).is_err());
            assert!(!dest.exists());
            assert!(others(dir.path(), &[]).is_empty());
        }

        #[test]
        fn a_group_failure_rolls_back_installed_members() {
            let dir = scratch();
            fs::write(dir.path().join("a.osm.pbf"), b"old a").unwrap();
            fs::write(dir.path().join("b.osm.pbf"), b"old b").unwrap();
            let (mut set, a, b) = staged_set(dir.path(), Overwrite::Replace);
            set.inject_fault(PublishFault::AfterInstalled(1));
            assert!(set.publish(&crate::host::Invocation::new()).is_err());
            assert_eq!(fs::read(&a).unwrap(), b"old a");
            assert_eq!(fs::read(&b).unwrap(), b"old b");
            assert!(others(dir.path(), &["a.osm.pbf", "b.osm.pbf"]).is_empty());

            // Refuse: nothing existed, so a rolled-back group leaves nothing.
            let dir = scratch();
            let (mut set, a, b) = staged_set(dir.path(), Overwrite::Refuse);
            set.inject_fault(PublishFault::AfterInstalled(0));
            assert!(set.publish(&crate::host::Invocation::new()).is_err());
            assert!(!a.exists() && !b.exists());
            assert!(others(dir.path(), &[]).is_empty());
        }

        /// A failure between moving the old output aside and installing the
        /// new one puts the old one back.
        #[test]
        fn a_failure_after_backup_restores_the_old_output() {
            let dir = scratch();
            let dest = dir.path().join("index");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("old.bin"), b"old").unwrap();
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None, &discard()).unwrap();
            stage_dir(&out, "new.bin", b"new");
            out.inject_fault(PublishFault::AfterBackup);
            assert!(out.publish(&crate::host::Invocation::new()).is_err());
            assert_eq!(fs::read(dest.join("old.bin")).unwrap(), b"old");
            assert!(!dest.join("new.bin").exists());
            assert!(others(dir.path(), &["index"]).is_empty());
        }

        /// When the rollback itself fails, the old output is kept - never
        /// deleted - and the error says where.
        #[test]
        fn a_failed_rollback_retains_the_old_output_and_names_it() {
            let dir = scratch();
            let a = dir.path().join("a.osm.pbf");
            let b = dir.path().join("b.osm.pbf");
            fs::write(&a, b"old a").unwrap();
            fs::write(&b, b"old b").unwrap();
            let (mut set, _, _) = staged_set(dir.path(), Overwrite::Replace);
            set.inject_fault(PublishFault::AfterInstalled(1));
            set.inject_fault(PublishFault::Restore);
            let err = set.publish(&crate::host::Invocation::new()).unwrap_err();
            let OutputError::RollbackIncomplete { unresolved, .. } = output_error(&err) else {
                panic!("expected an incomplete rollback, got {err}");
            };
            assert_eq!(unresolved.len(), 2, "{unresolved:?}");
            let backups: Vec<String> = others(dir.path(), &["a.osm.pbf", "b.osm.pbf"]);
            assert_eq!(backups.len(), 2, "{backups:?}");
            for backup in &backups {
                assert!(backup.starts_with(".pbfhogg-backup-"), "{backup}");
                assert!(err.to_string().contains(backup.as_str()), "{err}");
                let old: Vec<u8> = fs::read_dir(dir.path().join(backup))
                    .unwrap()
                    .map(|e| fs::read(e.unwrap().path()).unwrap())
                    .next()
                    .unwrap();
                assert!(old.starts_with(b"old "), "the old output survives");
            }
        }

        /// A directory whose swap fails and whose old generation cannot be
        /// put back keeps that old generation in its backup, and leaks no
        /// staging tree beside it.
        #[test]
        fn a_failed_directory_restore_keeps_the_backup_and_nothing_else() {
            let dir = scratch();
            let dest = dir.path().join("index");
            fs::create_dir(&dest).unwrap();
            fs::write(dest.join("old.bin"), b"old").unwrap();
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None, &discard()).unwrap();
            stage_dir(&out, "new.bin", b"new");
            out.inject_fault(PublishFault::AfterBackup);
            out.inject_fault(PublishFault::Restore);
            let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
            assert!(matches!(
                output_error(&err),
                OutputError::RollbackIncomplete { .. }
            ));
            let left = others(dir.path(), &[]);
            assert_eq!(left.len(), 1, "{left:?}");
            assert!(left[0].starts_with(".pbfhogg-backup-"), "{left:?}");
            assert!(err.to_string().contains(left[0].as_str()), "{err}");
        }

        /// A directory sync failure after the rename reports the output as
        /// published, and the output stays published.
        #[test]
        fn a_sync_failure_reports_a_published_output() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            let mut out = Output::new(&dest, Overwrite::Refuse, &discard()).unwrap();
            stage(&out, b"new").unwrap();
            out.inject_fault(PublishFault::DirSync);
            let err = out.publish(&crate::host::Invocation::new()).unwrap_err();
            assert!(matches!(output_error(&err), OutputError::Unsynced { .. }));
            assert!(err.to_string().contains("was published"), "{err}");
            assert_eq!(fs::read(&dest).unwrap(), b"new");
        }
    }
}
