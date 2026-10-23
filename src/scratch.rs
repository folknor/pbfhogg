//! Per-run scratch: where a command's disposable files live, how they are
//! cleaned up, and how the leftovers of runs that never got to clean up are
//! found.
//!
//! # The run directory
//!
//! A [`RunScratch`] is one directory, `.pbfhogg-run-<pid>-<128 random bits>`,
//! created exclusively (mode 0700) inside a caller-chosen parent. Everything
//! a run writes as disposable scratch goes inside it, under fixed simple
//! names: uniqueness comes from the directory, not from the names. One guard
//! owns it. Dropping the guard removes the directory; a removal that fails is
//! reported through the host's diagnostics with the path of what is left.
//! The public entry points that use scratch take the guard by value, so one
//! invocation owns exactly one directory and two invocations cannot share one.
//!
//! Outputs are not scratch. An output is staged beside its destination so it
//! can be renamed into place (`crate::output`); that placement is not
//! governed by the scratch parent.
//!
//! The scratch parent is trusted, as an output's parent is: it and the path
//! to it must stay stable for the run. The operations are path based.
//!
//! # Liveness
//!
//! The directory holds a `LOCK` file on which the guard keeps an exclusive
//! `flock(2)` for its whole life, and a `READY` marker created once that lock
//! is held. The kernel drops the lock when the process dies, so a lock that
//! can be taken is evidence the owner is gone that does not depend on pid
//! reuse. It is still only evidence: a forked child can keep the lock alive,
//! and network filesystems behave differently (below). A scanner takes the
//! lock only when `READY` exists, never creates `LOCK`, opens it read-write
//! (an exclusive lock needs a descriptor open for writing on NFS, where
//! `flock` is carried out as a whole-file POSIX lock), and releases it before
//! measuring, so scanning can never stall or fail a run that is starting.
//! Only "the lock was taken" counts as evidence the owner is gone; any probe
//! failure other than "held" (permissions, an unexpected file type, a lock
//! the filesystem refuses) leaves liveness unknown, and such a directory is
//! recorded as an issue, never as stale.
//!
//! Known limits on network filesystems, accepted:
//! - An NFS mount with `local_lock=flock` or `local_lock=all` (or an NFSv3
//!   mount with `nolock`) keeps `flock` local to each client. A live run on
//!   another host is then invisible: this host takes the lock, and the
//!   directory is reported as probably stale while it is in use.
//! - A client that loses its lock (an NFSv4 lease that expired while the
//!   client was cut off from the server) no longer protects its run
//!   directory, with the same result.
//!
//! The report only ever says "probably" and nothing is deleted. No probe can
//! establish liveness across hosts, so the scan does not try: the answer is
//! to point `--tmp-dir` at a local filesystem or to check a reported
//! directory before deleting it.
//!
//! # Finding leftovers
//!
//! [`find_stale`] lists what a crashed run left in one directory and
//! [`report_stale`] renders it as diagnostics. Nothing here ever deletes a
//! leftover. A `.pbfhogg-backup-*` entry can hold the only copy of a user's
//! previous output and is reported as its own kind.
//!
//! The exact names, and which of them an external tool may delete, are
//! defined in `reference/cli-reference.md`, "Names pbfhogg leaves behind".

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::host::{Diagnostics, Host};

const LOCK_NAME: &str = "LOCK";
const READY_NAME: &str = "READY";
const RUN_PREFIX: &str = ".pbfhogg-run-";
const STAGING_PREFIX: &str = ".pbfhogg-tmp-";
const BACKUP_PREFIX: &str = ".pbfhogg-backup-";

// ---------------------------------------------------------------------------
// The run directory
// ---------------------------------------------------------------------------

/// One run's scratch directory and the guard that removes it. Not `Clone`.
#[derive(Debug)]
pub struct RunScratch {
    path: PathBuf,
    owner: u128,
    lock: Option<File>,
    diagnostics: Diagnostics,
}

/// A path inside a [`RunScratch`], the only kind of path scratch files are
/// created at. Built by [`RunScratch::file`] and [`RunScratch::create_subdir`]
/// from a single path component, so it cannot name anything outside the run
/// directory, and it remembers which run issued it.
#[derive(Debug, Clone)]
pub(crate) struct ScratchPath {
    path: PathBuf,
    owner: u128,
}

impl std::ops::Deref for ScratchPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl RunScratch {
    /// Create a run directory inside `parent`.
    ///
    /// `parent` must exist and be a directory the caller can write; a failure
    /// names it. Nothing is created when `parent` is unusable.
    ///
    /// # Errors
    /// `parent` is missing, not a directory, or not writable, or the
    /// directory could not be initialised.
    pub fn create(parent: &Path, host: &Host) -> crate::Result<Self> {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let meta = fs::metadata(parent).map_err(|e| {
            crate::Error::from(e).context(format!(
                "scratch directory {} is unusable",
                parent.display()
            ))
        })?;
        if !meta.is_dir() {
            return Err(crate::Error::invalid_argument(format!(
                "scratch directory {} is not a directory",
                parent.display()
            )));
        }
        loop {
            let owner = random_u128()?;
            let path = parent.join(format!("{RUN_PREFIX}{}-{owner:032x}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    // The guard exists from here on, so a failure while
                    // initialising removes the directory it just made.
                    let mut run = Self {
                        path,
                        owner,
                        lock: None,
                        diagnostics: host.diagnostics.clone(),
                    };
                    run.initialise()?;
                    return Ok(run);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(crate::Error::from(e).context(format!(
                        "cannot create a scratch directory in {}",
                        parent.display()
                    )));
                }
            }
        }
    }

    fn initialise(&mut self) -> crate::Result<()> {
        let lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(LOCK_NAME))
            .map_err(|e| crate::Error::from(e).context("cannot create the scratch lock file"))?;
        // Nonblocking: nothing else has a reason to hold a lock on a file
        // that did not exist a moment ago, because scanners wait for READY.
        flock(&lock, libc::LOCK_EX | libc::LOCK_NB)
            .map_err(|e| crate::Error::from(e).context("cannot lock the scratch directory"))?;
        self.lock = Some(lock);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(READY_NAME))
            .map_err(|e| {
                crate::Error::from(e).context("cannot mark the scratch directory ready")
            })?;
        Ok(())
    }

    /// The run directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A scratch file path inside the run directory.
    pub(crate) fn file(&self, name: &str) -> crate::Result<ScratchPath> {
        let bad = name.is_empty()
            || name == "."
            || name == ".."
            || name == LOCK_NAME
            || name == READY_NAME
            || name.bytes().any(|b| b == b'/' || b == 0);
        if bad {
            return Err(crate::Error::internal(format!(
                "invalid scratch file name {name:?}"
            )));
        }
        Ok(ScratchPath {
            path: self.path.join(name),
            owner: self.owner,
        })
    }

    /// Create a directory inside the run directory.
    pub(crate) fn create_subdir(&self, name: &str) -> crate::Result<ScratchPath> {
        let dir = self.file(name)?;
        fs::create_dir(&dir.path).map_err(|e| {
            crate::Error::from(e).context(format!(
                "failed to create scratch directory {}",
                dir.path.display()
            ))
        })?;
        Ok(dir)
    }

    /// Delete a scratch file, returning the error. For the caller that needs
    /// the deletion to have happened (an anonymous store).
    pub(crate) fn unlink(&self, path: &ScratchPath) -> io::Result<()> {
        debug_assert_eq!(path.owner, self.owner);
        fs::remove_file(&path.path)
    }

    /// Delete a scratch file mid-run to give its space back. A file that is
    /// already gone is fine; any other failure is reported (with the path)
    /// and the run goes on, because the final cleanup tries again.
    pub(crate) fn remove(&self, path: &ScratchPath) {
        if path.owner != self.owner {
            crate::host::diag!(
                self.diagnostics,
                Warning,
                "scratch.remove",
                "refusing to remove {}: it belongs to another scratch directory",
                path.path.display()
            );
            return;
        }
        match fs::remove_file(&path.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => self.diagnostics.emit(
                crate::host::EventKind::Warning,
                "scratch.remove",
                format_args!(
                    "could not remove scratch file {}: {e}; it stays until the run's scratch \
                     directory is removed",
                    path.path.display()
                ),
            ),
        }
    }

    /// [`remove`](Self::remove) for a directory made by
    /// [`create_subdir`](Self::create_subdir), with everything in it.
    pub(crate) fn remove_tree(&self, path: &ScratchPath) {
        if path.owner != self.owner {
            crate::host::diag!(
                self.diagnostics,
                Warning,
                "scratch.remove",
                "refusing to remove {}: it belongs to another scratch directory",
                path.path.display()
            );
            return;
        }
        match fs::remove_dir_all(&path.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => self.diagnostics.emit(
                crate::host::EventKind::Warning,
                "scratch.remove",
                format_args!(
                    "could not remove scratch directory {}: {e}; it stays until the run's \
                     scratch directory is removed",
                    path.path.display()
                ),
            ),
        }
    }

    /// Remove everything in the run directory and then the directory. The
    /// lock and ready files go last, and only if everything else went, so a
    /// partial cleanup leaves a directory a later scan can still classify.
    fn cleanup(&mut self) {
        let result = remove_contents(&self.path).and_then(|()| {
            for name in [READY_NAME, LOCK_NAME] {
                match fs::remove_file(self.path.join(name)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            fs::remove_dir(&self.path)
        });
        // Close the lock last: a scanner never sees the directory unlocked
        // while it still holds scratch.
        self.lock = None;
        if let Err(e) = result {
            self.diagnostics.emit(
                crate::host::EventKind::Warning,
                "scratch.cleanup",
                format_args!(
                    "could not remove scratch directory {}: {e}; what is left there can be \
                     deleted by hand",
                    self.path.display()
                ),
            );
        }
    }
}

impl Drop for RunScratch {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn remove_contents(dir: &Path) -> io::Result<()> {
    let mut first_error = None;
    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        let name = entry.file_name();
        if name == OsStr::new(LOCK_NAME) || name == OsStr::new(READY_NAME) {
            continue;
        }
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = removed
            && e.kind() != io::ErrorKind::NotFound
        {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn random_u128() -> crate::Result<u128> {
    let mut bytes = [0_u8; 16];
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the pointer and length describe the unfilled tail of `bytes`.
        let n = unsafe {
            libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(crate::Error::from(e).context("no randomness for a scratch directory name"));
        }
        filled += usize::try_from(n).unwrap_or(0);
    }
    Ok(u128::from_le_bytes(bytes))
}

/// Create (or truncate) a scratch file for writing: a file the run itself
/// owns, never an output. The command layer may not call `File::create`
/// directly (outputs go through `crate::output`), so its scratch writes come
/// through here, where the distinction is stated.
pub(crate) fn create(path: &ScratchPath) -> io::Result<File> {
    File::create(&path.path)
}

/// [`create`], retaining the path in any error.
pub(crate) fn create_file(path: &ScratchPath) -> crate::Result<File> {
    create(path).map_err(|e| {
        crate::Error::from(e).context(format!(
            "failed to create scratch file {}",
            path.path.display()
        ))
    })
}

/// Open a scratch file the run wrote, for reading back. The caller names
/// the path in its error (it knows what the file holds); a `NotFound` stays
/// matchable for the scratch a stage may legitimately not have written.
#[allow(clippy::disallowed_methods)]
pub(crate) fn open(path: &ScratchPath) -> io::Result<File> {
    File::open(&path.path)
}

/// Open a scratch file for reading and writing, truncating it, or creating it
/// exclusively when `exclusive` is set. Scratch only, as for [`create`].
pub(crate) fn create_read_write(path: &ScratchPath, exclusive: bool) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    options.open(&path.path)
}

// ---------------------------------------------------------------------------
// Leftovers
// ---------------------------------------------------------------------------

/// What a leftover is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StaleKind {
    /// A [`RunScratch`] directory.
    RunDirectory,
    /// Scratch named by an older pbfhogg, beside an input or output.
    LegacyScratch,
    /// An output's hidden staging entry (`.pbfhogg-tmp-*`).
    OutputStaging,
    /// A kept copy of a previous output (`.pbfhogg-backup-*`). This can be
    /// the only copy there is. It is reported and never deleted.
    OutputBackup,
    /// Scratch an older geocode builder left inside an index directory
    /// (`.buckets-level*`). Found by [`find_stale_in_index`]. Replacing the
    /// index deletes it along with the old index, which is why it is its own
    /// kind: the other kinds are never deleted by anything.
    IndexScratch,
}

/// What was observed that makes a leftover probably stale. Each is an
/// observation, not a proof.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Observation {
    /// The directory's lock could be taken: no live owner holds it.
    LockAvailable,
    /// The ownership handshake is incomplete: `READY` or `LOCK` is absent.
    /// A run still starting or finishing, or one that died there.
    LockMissing,
    /// The pid in the name does not exist in this process namespace. A run in
    /// another container or namespace sharing the volume would look the same.
    PidAbsent(u32),
    /// The pid in the name exists here. Either a live run or a reused pid.
    PidPresent(u32),
    /// The name carries no pid.
    NoOwnerRecorded,
}

/// One probably-stale entry.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StaleScratch {
    pub path: PathBuf,
    pub kind: StaleKind,
    /// Logical bytes seen while walking the entry; an observation, since
    /// entries can change under the walk.
    pub observed_bytes: u64,
    /// Whether the walk reached every entry it tried to.
    pub walk_complete: bool,
    pub observation: Observation,
}

/// An entry that could not be inspected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanIssue {
    pub path: PathBuf,
    pub error: String,
}

/// The result of scanning one directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StaleScan {
    pub stale: Vec<StaleScratch>,
    pub issues: Vec<ScanIssue>,
}

impl fmt::Display for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockAvailable => f.write_str("its lock is not held"),
            Self::LockMissing => f.write_str(
                "its READY or LOCK file is missing, so its owner never completed or already \
                 undid the lock handshake: a run starting or finishing, or one that died there",
            ),
            Self::PidAbsent(pid) => write!(f, "no process {pid} exists here"),
            Self::PidPresent(pid) => write!(
                f,
                "process {pid} exists here, which may be a live run or a reused pid"
            ),
            Self::NoOwnerRecorded => f.write_str("it records no owner"),
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl fmt::Display for StaleScratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = format!(
            "{}{}",
            if self.walk_complete { "" } else { "at least " },
            human_bytes(self.observed_bytes)
        );
        let path = self.path.display();
        match self.kind {
            StaleKind::OutputBackup => write!(
                f,
                "{path} is a backup of a previous output that pbfhogg kept ({size}); it may be the \
                 only copy of that output, or belong to a publication still in progress ({}). \
                 pbfhogg never deletes it: check it, then remove it yourself",
                self.observation
            ),
            StaleKind::RunDirectory => write!(
                f,
                "{path} is probably scratch left by a pbfhogg run that did not finish ({size}; {}); \
                 pbfhogg does not remove it: delete it once no run is using it",
                self.observation
            ),
            StaleKind::LegacyScratch => write!(
                f,
                "{path} is probably scratch left by a pbfhogg run that did not finish ({size}; {}); \
                 its owner cannot be established. pbfhogg does not remove it: delete it once no \
                 run is using it",
                self.observation
            ),
            StaleKind::IndexScratch => write!(
                f,
                "{path} is probably scratch left inside a geocode index by an older pbfhogg run \
                 that did not finish ({size}; {}). pbfhogg does not remove it, but publishing a \
                 replacement index deletes the old index directory, and this with it",
                self.observation
            ),
            StaleKind::OutputStaging => write!(
                f,
                "{path} is probably the hidden staging copy of an output a pbfhogg run did not \
                 finish ({size}; {}); its owner cannot be established. pbfhogg does not remove \
                 it: delete it once no run is using it",
                self.observation
            ),
        }
    }
}

/// List what crashed runs left directly inside `dir`. Never deletes or
/// renames anything; an entry that cannot be inspected is recorded in
/// [`StaleScan::issues`] and the rest of the scan goes on.
#[must_use]
pub fn find_stale(dir: &Path) -> StaleScan {
    let mut scan = StaleScan::default();
    scan_dir(dir, false, &mut scan);
    scan.stale.sort_by(|a, b| a.path.cmp(&b.path));
    scan
}

/// Classify the entries directly inside `dir` into `scan`, and return the
/// unclassified subdirectories (never followed through a symlink) for a
/// caller that walks a tree. Classified entries are reported whole and never
/// descended into. With `inside_index`, `.buckets-level*` trees are reported
/// as [`StaleKind::IndexScratch`].
fn scan_dir(dir: &Path, inside_index: bool, scan: &mut StaleScan) -> Vec<PathBuf> {
    let mut subdirs = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            scan.issues.push(ScanIssue {
                path: dir.to_path_buf(),
                error: e.to_string(),
            });
            return subdirs;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                scan.issues.push(ScanIssue {
                    path: dir.to_path_buf(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        let name = entry.file_name();
        let path = entry.path();
        let (kind, observation) = if is_geocode_staging(&name) {
            let kind = classify_geocode_staging(&path, &mut scan.issues);
            // The digits in the name are a sequence counter, not a pid.
            (kind, Observation::NoOwnerRecorded)
        } else if let Some((kind, pid)) = classify(&name) {
            let observation = match kind {
                StaleKind::RunDirectory => match probe_lock(&path) {
                    Probe::Live => continue,
                    Probe::Observed(o) => o,
                    Probe::Unknown(detail) => {
                        scan.issues.push(ScanIssue {
                            path,
                            error: format!(
                                "whether a run is using this run directory is unknown \
                                 ({detail}); pbfhogg does not remove it, and it must not be \
                                 deleted while any run could be using it"
                            ),
                        });
                        continue;
                    }
                },
                _ => pid_observation(pid),
            };
            (kind, observation)
        } else if inside_index
            && name
                .as_bytes()
                .starts_with(LEGACY_BUCKETS_PREFIX.as_bytes())
        {
            (StaleKind::IndexScratch, Observation::NoOwnerRecorded)
        } else {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => subdirs.push(path),
                Ok(_) => {}
                Err(e) => scan.issues.push(ScanIssue {
                    path,
                    error: e.to_string(),
                }),
            }
            continue;
        };
        let (observed_bytes, walk_complete) = walk_size(&path);
        scan.stale.push(StaleScratch {
            path,
            kind,
            observed_bytes,
            walk_complete,
            observation,
        });
    }
    subdirs
}

/// The geocode builder before the output module staged a rebuild in
/// `<parent>/.geocode-build-<n>` (`new/` the generation being built, `old/`
/// the previous index while it was swapped), where `<n>` is a counter.
const LEGACY_GEOCODE_STAGING_PREFIX: &str = ".geocode-build-";

/// The `.buckets-level<N>` directories the geocode builder once made inside
/// the directory it was building.
const LEGACY_BUCKETS_PREFIX: &str = ".buckets-level";

fn is_geocode_staging(name: &OsStr) -> bool {
    name.as_bytes()
        .strip_prefix(LEGACY_GEOCODE_STAGING_PREFIX.as_bytes())
        .is_some_and(|rest| !rest.is_empty() && rest.iter().all(u8::is_ascii_digit))
}

/// Whether `name` is one of the names pbfhogg leaves behind (a run
/// directory, an output's staging or backup, an older version's scratch or
/// geocode staging). Matched on bytes: a staging or backup name keeps the
/// destination's tail, which need not be UTF-8. `.buckets-level*` is not
/// among them; it only means something inside an index.
pub(crate) fn is_leftover_name(name: &OsStr) -> bool {
    is_geocode_staging(name) || classify(name).is_some()
}

/// Every entry under `dir` (not `dir` itself) that no pbfhogg code path may
/// delete: anything [`is_leftover_name`] recognises, whatever its liveness.
/// Classified entries are listed whole and not descended into, nor are
/// `.buckets-level*` trees (index scratch, which replacing an index may
/// discard). Symlinks are never followed.
///
/// Used before anything is removed recursively: replacing a directory
/// output, removing a replaced output's backup, and dropping a directory
/// output's staging. A walk that cannot be completed is an error, because
/// what it did not see cannot be shown to be deletable.
pub(crate) fn protected_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next)? {
            let entry = entry?;
            let name = entry.file_name();
            if is_leftover_name(&name) {
                found.push(entry.path());
            } else if !name
                .as_bytes()
                .starts_with(LEGACY_BUCKETS_PREFIX.as_bytes())
                && entry.file_type()?.is_dir()
            {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// What an old geocode staging directory is. It can hold `old/`, which after
/// an interrupted swap was the only copy of the previous index, so anything
/// that cannot be shown to be plain scratch is classed as a backup. The
/// candidate itself is inspected first and never traversed when it is not a
/// real directory, so classification and measurement look at one object.
fn classify_geocode_staging(path: &Path, issues: &mut Vec<ScanIssue>) -> StaleKind {
    let mut uncertain = |error: String| {
        issues.push(ScanIssue {
            path: path.to_path_buf(),
            error,
        });
        StaleKind::OutputBackup
    };
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return StaleKind::OutputBackup,
        Err(e) => return uncertain(e.to_string()),
    }
    match fs::symlink_metadata(path.join("old")) {
        Ok(_) => StaleKind::OutputBackup,
        Err(e) if e.kind() == io::ErrorKind::NotFound => StaleKind::LegacyScratch,
        Err(e) => uncertain(e.to_string()),
    }
}

/// List what crashed runs left anywhere inside an index directory that is
/// about to be replaced: everything [`find_stale`] recognises, at any depth,
/// plus the `.buckets-level*` trees older geocode builders left there
/// ([`StaleKind::IndexScratch`]). Classified entries are reported whole and
/// not descended into.
///
/// Replacing an index refuses while any entry other than a `.buckets-level*`
/// tree is in it (`OutputError::HoldsLeftovers`, live run directories
/// included); the buckets are deleted with the old index, and the
/// [`StaleKind::IndexScratch`] wording says so. Nothing is deleted here.
/// `dir` is not followed if it is a symlink, and neither are its entries.
#[must_use]
pub fn find_stale_in_index(dir: &Path) -> StaleScan {
    let mut scan = StaleScan::default();
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        // Not a directory: nothing is inside it.
        Ok(_) => return scan,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return scan,
        Err(e) => {
            scan.issues.push(ScanIssue {
                path: dir.to_path_buf(),
                error: e.to_string(),
            });
            return scan;
        }
    }
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        pending.extend(scan_dir(&next, true, &mut scan));
    }
    scan.stale.sort_by(|a, b| a.path.cmp(&b.path));
    scan
}

/// Render a scan as diagnostics: one warning per leftover (backups under
/// their own key) and a note per entry that could not be inspected.
pub fn report_stale(diagnostics: &Diagnostics, scan: &StaleScan) {
    use crate::host::EventKind;
    for stale in &scan.stale {
        let key = if stale.kind == StaleKind::OutputBackup {
            "scratch.backup"
        } else {
            "scratch.stale"
        };
        diagnostics.emit(EventKind::Warning, key, format_args!("{stale}"));
    }
    for issue in &scan.issues {
        diagnostics.emit(
            EventKind::Note,
            "scratch.scan",
            format_args!(
                "could not inspect {}: {}",
                issue.path.display(),
                issue.error
            ),
        );
    }
}

/// The kind of leftover a directory entry name denotes, and the pid in it.
/// Matched on bytes, so a name whose tail is not UTF-8 is still recognised.
fn classify(name: &OsStr) -> Option<(StaleKind, Option<u32>)> {
    let name = name.as_bytes();
    for (prefix, kind) in [
        (RUN_PREFIX, StaleKind::RunDirectory),
        (STAGING_PREFIX, StaleKind::OutputStaging),
        (BACKUP_PREFIX, StaleKind::OutputBackup),
        (".pbfhogg-external-join-", StaleKind::LegacyScratch),
        (".pbfhogg-sparse-index-", StaleKind::LegacyScratch),
    ] {
        if let Some(rest) = name.strip_prefix(prefix.as_bytes()) {
            return Some((kind, leading_pid(rest)));
        }
    }
    for prefix in [
        "diff-par-",
        "derive-creates-",
        "derive-modifies-",
        "derive-deletes-",
        "derive-par-creates-",
        "derive-par-modifies-",
        "derive-par-deletes-",
    ] {
        if let Some(rest) = name.strip_prefix(prefix.as_bytes())
            && name.ends_with(b".tmp")
        {
            return Some((StaleKind::LegacyScratch, leading_pid(rest)));
        }
    }
    None
}

fn leading_pid(rest: &[u8]) -> Option<u32> {
    let len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&rest[..len])
        .ok()?
        .parse()
        .ok()
        .filter(|&pid| pid != 0 && i32::try_from(pid).is_ok())
}

fn pid_observation(pid: Option<u32>) -> Observation {
    let Some(pid) = pid else {
        return Observation::NoOwnerRecorded;
    };
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Observation::NoOwnerRecorded;
    };
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(raw, 0) } == 0 {
        return Observation::PidPresent(pid);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Observation::PidAbsent(pid),
        // Not permitted to signal it: it exists.
        _ => Observation::PidPresent(pid),
    }
}

enum Probe {
    /// The lock is held: a live run. Not reported.
    Live,
    /// Something was observed that makes the directory probably stale.
    Observed(Observation),
    /// The probe could not tell; the text says why. Never reported as stale.
    Unknown(String),
}

/// Probe a run directory's lock. Only a lock that could be taken
/// (`LockAvailable`) or a missing handshake file (`LockMissing`) counts as an
/// observation; every other outcome is `Unknown`, because a probe that
/// failed says nothing about the owner. See the module docs, "Liveness", for
/// what a successful probe cannot see on network filesystems.
fn probe_lock(dir: &Path) -> Probe {
    for name in [READY_NAME, LOCK_NAME] {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Probe::Unknown(format!("{} is not a regular file", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Probe::Observed(Observation::LockMissing);
            }
            Err(e) => return Probe::Unknown(format!("cannot inspect {}: {e}", path.display())),
        }
    }
    let lock = dir.join(LOCK_NAME);
    // Read-write, never create: on NFS `flock` is carried out as a POSIX
    // lock, and an exclusive one needs a descriptor open for writing.
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Probe::Observed(Observation::LockMissing);
        }
        Err(e) => {
            return Probe::Unknown(format!(
                "cannot open {} for writing to probe its lock: {e}",
                lock.display()
            ));
        }
    };
    // What was opened is what is judged: the check above can race with a
    // replacement of the file.
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Probe::Unknown(format!("{} is not a regular file", lock.display())),
        Err(e) => return Probe::Unknown(format!("cannot inspect {}: {e}", lock.display())),
    }
    match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
        // The lock is released as `file` drops, before anything is measured.
        Ok(()) => Probe::Observed(Observation::LockAvailable),
        Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Probe::Live,
        Err(e) => Probe::Unknown(format!(
            "probing the lock on {} failed: {e}",
            lock.display()
        )),
    }
}

/// Logical bytes under `path` and whether every step of the walk worked.
/// Symlinks are counted as themselves and never followed.
fn walk_size(path: &Path) -> (u64, bool) {
    let mut bytes = 0_u64;
    let mut complete = true;
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let meta = match fs::symlink_metadata(&next) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        bytes = bytes.saturating_add(meta.size());
        if !meta.is_dir() {
            continue;
        }
        match fs::read_dir(&next) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => pending.push(entry.path()),
                        Err(_) => complete = false,
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => complete = false,
        }
    }
    (bytes, complete)
}

/// Whether `inner` is `outer` or lies beneath it, comparing resolved
/// locations so a symlink cannot hide the overlap. A path that does not exist
/// cannot overlap anything.
///
/// # Errors
/// A path exists but cannot be resolved.
pub fn is_within(inner: &Path, outer: &Path) -> io::Result<bool> {
    let resolve = |path: &Path| match fs::canonicalize(path) {
        Ok(p) => Ok(Some(p)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    let (Some(inner), Some(outer)) = (resolve(inner)?, resolve(outer)?) else {
        return Ok(false);
    };
    Ok(inner.starts_with(&outer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host::for_tests()
    }

    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<(&'static str, String)>>);
    impl crate::host::DiagnosticSink for Recording {
        fn event(&self, event: &crate::host::Event<'_>) {
            self.0
                .lock()
                .expect("sink")
                .push((event.key, event.args.to_string()));
        }
    }

    fn recording(sink: &std::sync::Arc<Recording>) -> Diagnostics {
        Diagnostics::new(
            std::sync::Arc::clone(sink) as std::sync::Arc<dyn crate::host::DiagnosticSink>
        )
    }

    #[test]
    fn a_run_directory_is_private_unique_and_removed_on_drop() {
        let parent = crate::path_guard::tests::scratch();
        let a = RunScratch::create(parent.path(), &host()).expect("a");
        let b = RunScratch::create(parent.path(), &host()).expect("b");
        assert_ne!(a.path(), b.path());
        assert_eq!(a.path().parent(), Some(parent.path()));
        let mode = fs::metadata(a.path()).expect("meta").mode() & 0o777;
        assert_eq!(mode, 0o700);
        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        let file = a.file("data").expect("name");
        create(&file).expect("create");
        drop(a);
        assert!(!pa.exists());
        assert!(pb.exists());
        drop(b);
        assert_eq!(fs::read_dir(parent.path()).expect("dir").count(), 0);
    }

    /// The directory exists, locked and marked ready, from the moment
    /// `create` returns (ADR-0018: a bad location fails at startup, not
    /// mid-run), whether or not a file is ever made in it, and a run that
    /// used none still removes it.
    #[test]
    fn the_directory_exists_from_creation_and_is_removed_even_when_unused() {
        let parent = crate::path_guard::tests::scratch();
        let run = RunScratch::create(parent.path(), &host()).expect("run");
        let path = run.path().to_path_buf();
        assert!(path.is_dir());
        assert!(path.join(LOCK_NAME).is_file());
        assert!(path.join(READY_NAME).is_file());
        assert_eq!(fs::read_dir(parent.path()).expect("dir").count(), 1);
        drop(run);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(parent.path()).expect("dir").count(), 0);
    }

    /// A parent that cannot be written is refused by `create` itself.
    #[test]
    fn a_read_only_parent_is_refused_at_creation() {
        // Root passes the access check on anything.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = crate::path_guard::tests::scratch();
        fs::set_permissions(
            parent.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("chmod");
        let err = RunScratch::create(parent.path(), &host()).expect_err("read-only");
        assert!(
            err.to_string()
                .contains(&parent.path().display().to_string()),
            "{err}"
        );
        assert_eq!(fs::read_dir(parent.path()).expect("dir").count(), 0);
        fs::set_permissions(
            parent.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod back");
    }

    #[test]
    fn an_unusable_parent_is_refused_and_creates_nothing() {
        let parent = crate::path_guard::tests::scratch();
        let missing = parent.path().join("missing");
        let err = RunScratch::create(&missing, &host()).expect_err("missing");
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(!missing.exists());
        let file = parent.path().join("file");
        fs::write(&file, b"x").expect("file");
        let err = RunScratch::create(&file, &host()).expect_err("not a directory");
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn scratch_names_are_single_components() {
        let parent = crate::path_guard::tests::scratch();
        let run = RunScratch::create(parent.path(), &host()).expect("run");
        for bad in ["", ".", "..", "a/b", "/abs", LOCK_NAME, READY_NAME, "nul\0"] {
            assert!(run.file(bad).is_err(), "{bad:?}");
        }
        assert!(run.file("id-W0-007").is_ok());
    }

    #[test]
    fn removal_of_a_foreign_path_is_refused() {
        let parent = crate::path_guard::tests::scratch();
        let sink = std::sync::Arc::new(Recording::default());
        let mut h = host();
        h.diagnostics = recording(&sink);
        let a = RunScratch::create(parent.path(), &h).expect("a");
        let b = RunScratch::create(parent.path(), &h).expect("b");
        let theirs = b.file("keep").expect("name");
        create(&theirs).expect("create");
        a.remove(&theirs);
        assert!(theirs.exists());
        assert_eq!(sink.0.lock().expect("sink").len(), 1);
    }

    #[test]
    fn a_live_run_is_not_reported_and_a_dead_one_is() {
        let parent = crate::path_guard::tests::scratch();
        let live = RunScratch::create(parent.path(), &host()).expect("live");
        let scan = find_stale(parent.path());
        assert!(scan.stale.is_empty(), "{scan:?}");
        // The same layout without a holder: the directory of a crashed run.
        let dead = parent.path().join(".pbfhogg-run-999999-00");
        fs::create_dir(&dead).expect("dir");
        fs::write(dead.join(LOCK_NAME), b"").expect("lock");
        fs::write(dead.join(READY_NAME), b"").expect("ready");
        fs::write(dead.join("id-W0-000"), vec![0_u8; 4096]).expect("shard");
        let scan = find_stale(parent.path());
        assert_eq!(scan.stale.len(), 1, "{scan:?}");
        let found = &scan.stale[0];
        assert_eq!(found.kind, StaleKind::RunDirectory);
        assert_eq!(found.path, dead);
        assert_eq!(found.observation, Observation::LockAvailable);
        assert!(found.observed_bytes >= 4096);
        assert!(found.walk_complete);
        drop(live);
        assert!(dead.exists(), "detection never deletes");
    }

    #[test]
    fn an_unfinished_run_directory_is_an_uncertain_observation() {
        let parent = crate::path_guard::tests::scratch();
        fs::create_dir(parent.path().join(".pbfhogg-run-1-ab")).expect("dir");
        let scan = find_stale(parent.path());
        assert_eq!(scan.stale.len(), 1);
        assert_eq!(scan.stale[0].observation, Observation::LockMissing);
    }

    #[test]
    fn legacy_staging_and_backup_leftovers_are_typed() {
        let parent = crate::path_guard::tests::scratch();
        let dir = parent.path();
        fs::create_dir(dir.join(".pbfhogg-external-join-999999")).expect("legacy dir");
        fs::write(dir.join(".pbfhogg-sparse-index-999999-2"), b"x").expect("legacy file");
        fs::write(dir.join("diff-par-999999-n-0.txt.tmp"), b"x").expect("diff shard");
        fs::write(dir.join("derive-creates-999999.xml.tmp"), b"x").expect("derive");
        fs::write(dir.join("derive-par-modifies-999999-ab-n-0.xml.tmp"), b"x").expect("par");
        fs::write(dir.join(".pbfhogg-tmp-999999-0.out.pbf"), b"abc").expect("staging");
        fs::write(
            dir.join(format!("{BACKUP_PREFIX}999999-0.out.pbf")),
            b"previous",
        )
        .expect("backup");
        fs::write(dir.join("diff-par-notes.txt"), b"mine").expect("unrelated");
        fs::write(dir.join("out.pbf"), b"mine").expect("unrelated");
        let scan = find_stale(dir);
        assert!(scan.issues.is_empty(), "{scan:?}");
        let count = |kind| scan.stale.iter().filter(|s| s.kind == kind).count();
        assert_eq!(count(StaleKind::LegacyScratch), 5, "{scan:?}");
        assert_eq!(count(StaleKind::OutputStaging), 1);
        assert_eq!(count(StaleKind::OutputBackup), 1);
        let backup = scan
            .stale
            .iter()
            .find(|s| s.kind == StaleKind::OutputBackup)
            .expect("backup");
        assert_eq!(backup.observation, Observation::PidAbsent(999_999));
        assert_eq!(backup.observed_bytes, 8);
        let text = backup.to_string();
        assert!(text.contains("only copy"), "{text}");
        assert!(text.contains("never deletes"), "{text}");
        // Reporting renders every entry with distinct keys, and mutates nothing.
        let sink = std::sync::Arc::new(Recording::default());
        report_stale(&recording(&sink), &scan);
        let events = sink.0.lock().expect("sink");
        assert_eq!(events.len(), 7);
        assert_eq!(
            events
                .iter()
                .filter(|(k, _)| *k == "scratch.backup")
                .count(),
            1
        );
        assert_eq!(
            fs::read(dir.join(format!("{BACKUP_PREFIX}999999-0.out.pbf"))).expect("backup"),
            b"previous"
        );
    }

    #[test]
    fn old_geocode_staging_is_told_apart_by_whether_it_holds_the_old_index() {
        let parent = crate::path_guard::tests::scratch();
        let dir = parent.path();
        // A build that died: scratch only.
        let plain = dir.join(".geocode-build-3");
        fs::create_dir_all(plain.join("new").join(".buckets-level17")).expect("dirs");
        fs::write(plain.join("new").join(".buckets-level17").join("b"), b"x").expect("bucket");
        // A swap that died: the previous index may be the only copy.
        let swap = dir.join(".geocode-build-4");
        fs::create_dir_all(swap.join("old")).expect("dirs");
        fs::write(swap.join("old").join("header"), b"index").expect("old index");
        // A link is judged without being followed.
        let target = dir.join("elsewhere");
        fs::create_dir_all(target.join("old")).expect("dirs");
        std::os::unix::fs::symlink(&target, dir.join(".geocode-build-5")).expect("link");
        // Not a staging name.
        fs::create_dir(dir.join(".geocode-build-notes")).expect("dir");
        fs::create_dir(dir.join(".geocode-build-")).expect("dir");

        let scan = find_stale(dir);
        assert!(scan.issues.is_empty(), "{scan:?}");
        let kinds: Vec<_> = scan
            .stale
            .iter()
            .map(|s| (s.path.clone(), s.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (plain.clone(), StaleKind::LegacyScratch),
                (swap.clone(), StaleKind::OutputBackup),
                (dir.join(".geocode-build-5"), StaleKind::OutputBackup),
            ]
        );
        // The digits are a counter: no pid is read out of them.
        assert!(
            scan.stale
                .iter()
                .all(|s| s.observation == Observation::NoOwnerRecorded)
        );
        let swap_text = scan.stale[1].to_string();
        assert!(swap_text.contains("only copy"), "{swap_text}");
        assert!(
            plain
                .join("new")
                .join(".buckets-level17")
                .join("b")
                .exists()
        );
        assert!(swap.join("old").join("header").exists());
    }

    #[test]
    fn scratch_inside_an_index_directory_is_found_and_left_alone() {
        let parent = crate::path_guard::tests::scratch();
        let index = parent.path().join("index");
        fs::create_dir_all(index.join(".buckets-level17")).expect("dirs");
        fs::write(index.join(".buckets-level17").join("b"), vec![0_u8; 300]).expect("bucket");
        fs::create_dir(index.join(".buckets-level21")).expect("dir");
        fs::write(index.join("header"), b"index").expect("index file");
        fs::write(index.join("buckets-level-notes"), b"mine").expect("unrelated");

        let scan = find_stale_in_index(&index);
        assert!(scan.issues.is_empty(), "{scan:?}");
        assert_eq!(scan.stale.len(), 2, "{scan:?}");
        assert!(scan.stale.iter().all(|s| s.kind == StaleKind::IndexScratch));
        assert_eq!(scan.stale[0].path, index.join(".buckets-level17"));
        assert!(scan.stale[0].observed_bytes >= 300);
        let text = scan.stale[0].to_string();
        assert!(text.contains("deletes the old index"), "{text}");
        assert!(index.join(".buckets-level17").join("b").exists());
        assert!(index.join("buckets-level-notes").exists());

        // The plain scan of the same directory does not know these names.
        assert!(find_stale(&index).stale.is_empty());
        // Missing, plain-file and symlinked indexes hold nothing to report.
        assert!(
            find_stale_in_index(&parent.path().join("missing"))
                .stale
                .is_empty()
        );
        assert!(find_stale_in_index(&index.join("header")).stale.is_empty());
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&index, &link).expect("link");
        assert!(find_stale_in_index(&link).stale.is_empty());
    }

    /// Inside an index the scan goes to any depth and recognises every kind,
    /// not only the buckets; what it classifies it does not descend into.
    #[test]
    fn the_index_scan_classifies_everything_at_any_depth() {
        let parent = crate::path_guard::tests::scratch();
        let index = parent.path().join("index");
        let deep = index.join("a").join("b");
        fs::create_dir_all(&deep).expect("dirs");
        let backup = deep.join(".pbfhogg-backup-999999-0.x");
        fs::create_dir_all(backup.join(".pbfhogg-tmp-999999-1.y")).expect("backup");
        let staging = index.join(".pbfhogg-tmp-999999-2.z");
        fs::write(&staging, b"x").expect("staging");
        let buckets = index.join("a").join(".buckets-level9");
        fs::create_dir_all(&buckets).expect("buckets");
        let scan = find_stale_in_index(&index);
        assert!(scan.issues.is_empty(), "{scan:?}");
        let found: Vec<_> = scan
            .stale
            .iter()
            .map(|s| (s.path.clone(), s.kind))
            .collect();
        assert_eq!(
            found,
            [
                (staging.clone(), StaleKind::OutputStaging),
                (buckets.clone(), StaleKind::IndexScratch),
                (backup.clone(), StaleKind::OutputBackup),
            ]
        );
        // The protected walk sees the same entries but the buckets.
        assert_eq!(
            protected_entries(&index).expect("walk"),
            [staging, backup.clone()]
        );
        assert!(backup.exists());
    }

    /// Names are matched on bytes: a staging or backup name keeps the
    /// destination's tail, which need not be UTF-8.
    #[test]
    fn a_name_that_is_not_utf8_is_still_classified() {
        let mut name = b".pbfhogg-backup-999999-0.".to_vec();
        name.push(0xff);
        let name = OsStr::from_bytes(&name);
        assert_eq!(
            classify(name),
            Some((StaleKind::OutputBackup, Some(999_999)))
        );
        assert!(is_leftover_name(name));
        let mut legacy = b"diff-par-12-\xfe".to_vec();
        legacy.extend_from_slice(b".tmp");
        assert_eq!(
            classify(OsStr::from_bytes(&legacy)),
            Some((StaleKind::LegacyScratch, Some(12)))
        );
        assert!(!is_leftover_name(OsStr::new(".buckets-level17")));
    }

    /// A lock that cannot be probed says nothing about the owner: the
    /// directory is an issue that says so, never a stale entry.
    #[test]
    fn a_lock_that_cannot_be_probed_leaves_liveness_unknown() {
        // Root opens anything, so the probe cannot fail there.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = crate::path_guard::tests::scratch();
        let run = parent.path().join(".pbfhogg-run-999999-00");
        fs::create_dir(&run).expect("dir");
        fs::write(run.join(LOCK_NAME), b"").expect("lock");
        fs::write(run.join(READY_NAME), b"").expect("ready");
        // Not writable: the read-write probe cannot open it.
        fs::set_permissions(
            run.join(LOCK_NAME),
            std::os::unix::fs::PermissionsExt::from_mode(0o400),
        )
        .expect("chmod");
        let scan = find_stale(parent.path());
        assert!(scan.stale.is_empty(), "{scan:?}");
        assert_eq!(scan.issues.len(), 1, "{scan:?}");
        assert_eq!(scan.issues[0].path, run);
        assert!(scan.issues[0].error.contains("unknown"), "{scan:?}");
        assert!(
            scan.issues[0].error.contains("must not be deleted"),
            "{scan:?}"
        );
    }

    /// The lock a live run holds is seen through a read-write probe. (On a
    /// local filesystem a read-only one would see it too; on NFS it would
    /// fail with EBADF, which is why the probe opens for writing.)
    #[test]
    fn the_probe_opens_the_lock_for_writing() {
        let parent = crate::path_guard::tests::scratch();
        let live = RunScratch::create(parent.path(), &host()).expect("live");
        assert!(matches!(probe_lock(live.path()), Probe::Live));
        let src = include_str!("scratch.rs");
        let probe = src
            .split("fn probe_lock(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("probe_lock source");
        assert!(
            probe.contains(".write(true)"),
            "the probe must open read-write"
        );
        assert!(
            !probe.contains(".create("),
            "the probe must never create LOCK"
        );
    }

    #[test]
    fn a_pid_that_exists_is_reported_as_present_not_as_stale_proof() {
        let parent = crate::path_guard::tests::scratch();
        let own = format!("{STAGING_PREFIX}{}-0.out.pbf", std::process::id());
        fs::write(parent.path().join(own), b"x").expect("staging");
        let scan = find_stale(parent.path());
        assert_eq!(
            scan.stale[0].observation,
            Observation::PidPresent(std::process::id())
        );
    }

    #[test]
    fn an_unreadable_directory_is_an_issue_not_a_failure() {
        let parent = crate::path_guard::tests::scratch();
        let scan = find_stale(&parent.path().join("missing"));
        assert!(scan.stale.is_empty());
        assert_eq!(scan.issues.len(), 1);
    }

    #[test]
    fn a_failed_cleanup_is_reported_with_the_leftover_path() {
        // Root ignores directory permissions, so this check cannot fail there.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let parent = crate::path_guard::tests::scratch();
        let sink = std::sync::Arc::new(Recording::default());
        let mut h = host();
        h.diagnostics = recording(&sink);
        let run = RunScratch::create(parent.path(), &h).expect("run");
        let path = run.path().to_path_buf();
        let sub = run.create_subdir("stuck").expect("subdir");
        fs::write(sub.join("f"), b"x").expect("file");
        fs::set_permissions(
            &sub.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("chmod");
        drop(run);
        let events = sink.0.lock().expect("sink").clone();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, "scratch.cleanup");
        assert!(
            events[0].1.contains(&path.display().to_string()),
            "{events:?}"
        );
        // The lock and ready files were kept, so a later scan can classify it.
        assert!(path.join(LOCK_NAME).exists());
        let scan = find_stale(parent.path());
        assert_eq!(scan.stale[0].observation, Observation::LockAvailable);
        fs::set_permissions(
            &sub.path,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod back");
    }

    #[test]
    fn within_resolves_symlinks() {
        let parent = crate::path_guard::tests::scratch();
        let outer = parent.path().join("outer");
        fs::create_dir_all(outer.join("inner")).expect("dirs");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(outer.join("inner"), &link).expect("link");
        assert!(is_within(&link, &outer).expect("within"));
        assert!(is_within(&outer, &outer).expect("same"));
        assert!(!is_within(&outer, &link).expect("outside"));
        assert!(!is_within(&parent.path().join("nope"), &outer).expect("missing"));
    }
}
