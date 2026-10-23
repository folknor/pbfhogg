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
//!   without publishing removes the staging entry, so a failed, panicking or
//!   dropped run leaves nothing at the destination and leaves a pre-existing
//!   destination byte-identical. A run killed outright (SIGKILL, power loss)
//!   skips the cleanup and can leave a hidden `.pbfhogg-tmp-*` sibling, never
//!   a file under the name the caller chose.
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
//!
//! # Concurrency
//!
//! Under `Refuse` the guarantee holds against concurrent processes: the
//! final step is `renameat2(RENAME_NOREPLACE)` or, for files where that is
//! unavailable, `link(2)`, both of which fail rather than replace. Under
//! `Replace` the destination is re-inspected immediately before the rename;
//! another process mutating the destination entry in the instant between
//! that inspection and the rename is outside the contract (the rename never
//! follows a link, so nothing is written through one). Nothing here makes a
//! group of outputs visible to readers atomically.
//!
//! # Shapes
//!
//! [`Output`] is one file, [`OutputDir`] one directory (the geocode index),
//! [`OutputSet`] a group of files published together (`extract --config`).
//! A group publishes all or nothing as far as failures allow: if a member
//! fails to install, members already installed are rolled back, and a
//! rollback step that cannot complete keeps the previous output in a
//! `.pbfhogg-backup-*` sibling and says so in the error.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, OutputError, Result, new_output_error};

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

/// Apply the destination rules to whatever is at `dest` now.
fn inspect(
    dest: &Path,
    shape: Shape,
    overwrite: Overwrite,
    validator: Option<Validator>,
) -> Result<()> {
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
}

impl Target {
    fn prepare(
        dest: &Path,
        shape: Shape,
        overwrite: Overwrite,
        validator: Option<Validator>,
    ) -> Result<Self> {
        let dest = normalize(dest)?;
        inspect(&dest, shape, overwrite, validator)?;
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
                    drop(fs::remove_dir_all(&staging));
                    return Err(Error::from(e).context(format!(
                        "cannot create a staging directory for output {}",
                        dest.display()
                    )));
                }
                (staging, write)
            }
        };
        Ok(Self {
            shape,
            dest,
            staging,
            write,
            overwrite,
            validator,
            state: State::Staged,
            linked: false,
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
    fn install(&mut self, use_backup: bool, faults: &Faults) -> Result<()> {
        let identity = self.identity();
        match self.overwrite {
            Overwrite::Refuse => self.install_exclusive(identity, faults),
            Overwrite::Replace => self.install_replacing(identity, use_backup, faults),
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
                drop(fs::remove_dir(&container));
                return Err(Error::from(e).context(format!(
                    "cannot move existing output {} aside",
                    self.dest.display()
                )));
            }
            self.state = State::BackedUp(Backup { container, entry });
            if fault_is!(faults, PublishFault::AfterBackup) {
                return Err(injected("after backup"));
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
                    drop(fs::remove_dir(&backup.container));
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
                        drop(fs::remove_dir(&backup.container));
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

    /// Mark published and drop what publication no longer needs. Cleanup is
    /// best effort: the output is already in place.
    fn commit(&mut self) {
        if let State::Installed {
            backup: Some(backup),
            ..
        } = std::mem::replace(&mut self.state, State::Committed)
        {
            drop(fs::remove_dir_all(&backup.container));
        }
        if self.shape == Shape::Dir {
            drop(fs::remove_dir(&self.staging));
        }
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        match self.state {
            // Only ever the staging entry this run reserved. A backup is a
            // separate `.pbfhogg-backup-*` sibling and is never touched here:
            // after a failed restore it may be the only copy of the old
            // output.
            //
            // A directory's staging container only ever holds this run's
            // new generation (or nothing, once it was moved into place), so
            // it is removed whole in every state but `Committed`.
            State::Staged | State::BackedUp(_) | State::Installed { .. } => match self.shape {
                Shape::Dir => drop(fs::remove_dir_all(&self.staging)),
                // After a plain rename the staging name is gone; it is still
                // there after a link install (removing that name leaves the
                // published output intact) and after a failed install.
                Shape::File => {
                    if matches!(self.state, State::Staged) || self.write.exists() {
                        drop(fs::remove_file(&self.write));
                    }
                }
            },
            // A link-installed file whose staging name could not be unlinked
            // at publication gets one more try here.
            State::Committed => {
                if self.linked {
                    drop(fs::remove_file(&self.write));
                }
            }
        }
    }
}

/// Install every member, rolling back on the first failure, then commit and
/// sync. The one publication routine behind every shape.
fn publish_all(targets: &mut [Target], faults: &Faults) -> Result<()> {
    for target in targets.iter() {
        target.recheck()?;
    }
    // A directory's files were written by whatever filled it, with no sync
    // of their own; make the generation durable before it replaces anything,
    // so a delayed write error stops the publication instead of surfacing
    // after the previous output is gone.
    for target in targets.iter() {
        if target.shape == Shape::Dir {
            sync_tree(&target.write).map_err(|e| {
                Error::from(e).context(format!(
                    "cannot sync the new contents of {} before publishing it",
                    target.dest.display()
                ))
            })?;
        }
    }
    if fault_is!(faults, PublishFault::BeforeInstall) {
        return Err(injected("before install"));
    }
    let group = targets.len() > 1;
    for i in 0..targets.len() {
        let use_backup = group || targets[i].shape == Shape::Dir;
        let mut result = targets[i].install(use_backup, faults);
        if result.is_ok() && fault_is!(faults, PublishFault::AfterInstalled(i)) {
            result = Err(injected("after a member was installed"));
        }
        if let Err(error) = result {
            let unresolved: Vec<String> = targets[..=i]
                .iter_mut()
                .rev()
                .filter_map(|target| target.rollback(faults))
                .collect();
            if unresolved.is_empty() {
                return Err(error);
            }
            return Err(new_output_error(OutputError::RollbackIncomplete {
                unresolved,
                source: error,
            }));
        }
    }
    for target in targets.iter_mut() {
        target.commit();
    }
    // The renames are done and stay done; a sync failure only means their
    // durability is unconfirmed, and says so.
    let mut synced: Vec<&Path> = Vec::new();
    for target in targets.iter() {
        let parent = parent_dir(&target.dest);
        if synced.contains(&parent) {
            continue;
        }
        synced.push(parent);
        let result = if fault_is!(faults, PublishFault::DirSync) {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected directory sync fault",
            ))
        } else {
            sync_dir(parent)
        };
        if let Err(source) = result {
            return Err(new_output_error(OutputError::Unsynced {
                path: target.dest.clone(),
                source,
            }));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Sync every regular file under `dir`, then every directory, the root
/// last.
fn sync_tree(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            fs::File::open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(dir)
}
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Public shapes
// ---------------------------------------------------------------------------

/// One output file.
///
/// Construct it before any work: [`Output::new`] applies the destination
/// rules and reserves the staging file. Write to [`Output::path`], then
/// [`Output::publish`]. Library command entry points take an `Output` by
/// value and publish it themselves once they have succeeded.
#[derive(Debug)]
pub struct Output {
    target: Target,
    #[cfg(feature = "test-hooks")]
    faults: Faults,
}

impl Output {
    /// Check `destination` against the output rules and reserve its staging
    /// file.
    ///
    /// # Errors
    /// [`OutputError::Exists`] for an existing destination under
    /// [`Overwrite::Refuse`]; [`OutputError::Symlink`] for a symlink;
    /// [`OutputError::NotAFile`] for a directory or special file;
    /// [`OutputError::InvalidPath`] for a path that names no file; an I/O
    /// error if the staging file cannot be created (the directory is missing
    /// or not writable).
    pub fn new(destination: impl AsRef<Path>, overwrite: Overwrite) -> Result<Self> {
        Ok(Self {
            target: Target::prepare(destination.as_ref(), Shape::File, overwrite, None)?,
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

    /// The staging file to write. It exists (empty) from construction until
    /// publication or drop.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.target.write
    }

    /// The overwrite intent this output was built with.
    #[must_use]
    pub fn overwrite(&self) -> Overwrite {
        self.target.overwrite
    }

    /// Open the staging file for writing, truncated. The one way a command
    /// that is not a `PbfWriter` (OSC, diff text, export) gets its output
    /// file.
    ///
    /// # Errors
    /// Any I/O error opening the staging file, naming the destination.
    pub fn create(&self) -> Result<fs::File> {
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.target.write)
            .map_err(|e| {
                Error::from(e).context(format!(
                    "cannot open the staging file for output {}",
                    self.target.dest.display()
                ))
            })
    }

    /// Rename the staging file into place. Call only after the writer has
    /// been flushed and every check on the output has passed.
    ///
    /// # Errors
    /// The destination rules are re-applied first; under
    /// [`Overwrite::Refuse`] a destination that appeared meanwhile is
    /// refused exclusively. [`OutputError::Unsynced`] means the output *was*
    /// published and only the directory sync failed.
    pub fn publish(mut self) -> Result<()> {
        let faults = self.faults();
        publish_all(std::slice::from_mut(&mut self.target), &faults)
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
    /// [`OutputError::MountPoint`] in place of `NotAFile`, plus whatever
    /// `validator` refuses.
    pub fn new(
        destination: impl AsRef<Path>,
        overwrite: Overwrite,
        validator: Option<Validator>,
    ) -> Result<Self> {
        Ok(Self {
            target: Target::prepare(destination.as_ref(), Shape::Dir, overwrite, validator)?,
            #[cfg(feature = "test-hooks")]
            faults: Faults::new(),
        })
    }

    /// The directory the caller named. Never write here.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.target.dest
    }

    /// The staging directory to fill. It exists (empty) from construction.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.target.write
    }

    /// Swap the staging directory into place. An existing destination is
    /// moved to a backup first and restored if the swap fails.
    ///
    /// # Errors
    /// As [`Output::publish`]; [`OutputError::RollbackIncomplete`] if the
    /// swap failed and the previous directory could not be put back (the
    /// error names where it is kept).
    pub fn publish(mut self) -> Result<()> {
        #[cfg(feature = "test-hooks")]
        let faults = self.faults.clone();
        #[cfg(not(feature = "test-hooks"))]
        let faults = Faults;
        publish_all(std::slice::from_mut(&mut self.target), &faults)
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
    /// Check every destination and reserve a staging file for each.
    ///
    /// # Errors
    /// Any member's [`Output::new`] error, or
    /// [`OutputError::Collision`] when two destinations name one file (the
    /// same canonical directory and name, or an existing hard link).
    pub fn files<I, P>(destinations: I, overwrite: Overwrite) -> Result<Self>
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
            .map(|path| Output::new(path, overwrite))
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
    /// # Errors
    /// As [`Output::publish`], plus [`OutputError::RollbackIncomplete`]
    /// naming every member whose previous state could not be restored.
    pub fn publish(mut self) -> Result<()> {
        #[cfg(feature = "test-hooks")]
        let faults = self.faults.clone();
        #[cfg(not(feature = "test-hooks"))]
        let faults = Faults;
        let mut targets: Vec<Target> = Vec::with_capacity(self.members.len());
        for member in self.members.drain(..) {
            targets.push(member.into_target());
        }
        publish_all(&mut targets, &faults)
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

    #[test]
    fn a_fresh_output_publishes_and_leaves_nothing_else() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        let out = Output::new(&dest, Overwrite::Refuse).unwrap();
        assert!(!dest.exists(), "nothing at the destination before publish");
        fs::write(out.path(), b"new").unwrap();
        out.publish().unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new");
        assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
    }

    #[test]
    fn an_existing_output_is_refused_at_construction() {
        let dir = scratch();
        let dest = dir.path().join("out.osm.pbf");
        fs::write(&dest, b"user data").unwrap();
        let err = Output::new(&dest, Overwrite::Refuse).unwrap_err();
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
        let out = Output::new(&dest, Overwrite::Replace).unwrap();
        fs::write(out.path(), b"new").unwrap();
        out.publish().unwrap();
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
            let out = Output::new(&dest, Overwrite::Replace).unwrap();
            fs::write(out.path(), b"complete-looking output").unwrap();
        }
        assert_eq!(fs::read(&dest).unwrap(), b"user data");
        assert!(others(dir.path(), &["important.json"]).is_empty());

        let fresh = dir.path().join("fresh.json");
        {
            let out = Output::new(&fresh, Overwrite::Refuse).unwrap();
            fs::write(out.path(), b"complete-looking output").unwrap();
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
        let out = Output::new(&dest, Overwrite::Refuse).unwrap();
        fs::write(out.path(), b"ours").unwrap();
        fs::write(&dest, b"theirs").unwrap();
        let err = out.publish().unwrap_err();
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
                let err = Output::new(&spelling, overwrite).unwrap_err();
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
        let err = OutputDir::new(&dir_link, Overwrite::Replace, None).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Symlink { .. }));
    }

    #[test]
    fn the_wrong_kind_is_refused_even_under_replace() {
        let dir = scratch();
        let a_dir = dir.path().join("a-dir");
        fs::create_dir(&a_dir).unwrap();
        let err = Output::new(&a_dir, Overwrite::Replace).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::NotAFile { .. }));

        let a_file = dir.path().join("a-file");
        fs::write(&a_file, b"x").unwrap();
        let err = OutputDir::new(&a_file, Overwrite::Replace, None).unwrap_err();
        assert!(matches!(
            output_error(&err),
            OutputError::NotADirectory { .. }
        ));

        #[cfg(unix)]
        {
            let err = Output::new("/dev/null", Overwrite::Replace).unwrap_err();
            assert!(matches!(output_error(&err), OutputError::NotAFile { .. }));
        }
    }

    #[test]
    fn a_path_naming_nothing_is_refused() {
        for path in ["..", "/", "a/.."] {
            let err = Output::new(path, Overwrite::Replace).unwrap_err();
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
        let err = Output::new(&dest, Overwrite::Refuse).unwrap_err();
        assert!(err.report().contains("out.osm.pbf"), "{}", err.report());
    }

    /// Writers that pick an encoding from the name's tail (merge-changes
    /// gzips a `.gz`) see the same tail on the staging file, and a long
    /// UTF-8 name stays valid UTF-8 when shortened.
    #[test]
    fn the_staging_name_keeps_the_suffix() {
        let dir = scratch();
        let dest = dir.path().join("changes.osc.gz");
        let out = Output::new(&dest, Overwrite::Refuse).unwrap();
        let name = out.path().file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(".pbfhogg-tmp-") && name.ends_with(".osc.gz"),
            "{name}"
        );
        assert_eq!(out.path().parent(), dest.parent());
        drop(out);

        let long = format!("{}.osc.gz", "\u{e9}".repeat(120));
        let dest = dir.path().join(&long);
        let out = Output::new(&dest, Overwrite::Refuse).unwrap();
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
        let out = OutputDir::new(&dest, Overwrite::Replace, None).unwrap();
        fs::write(out.path().join("new.bin"), b"new").unwrap();
        out.publish().unwrap();
        assert!(!dest.join("old.bin").exists());
        assert_eq!(fs::read(dest.join("new.bin")).unwrap(), b"new");
        assert!(others(dir.path(), &["index"]).is_empty());

        let err = OutputDir::new(&dest, Overwrite::Refuse, None).unwrap_err();
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
        assert!(OutputDir::new(&dest, Overwrite::Replace, Some(refuse_marked)).is_err());
        fs::remove_file(dest.join("marker")).unwrap();
        let out = OutputDir::new(&dest, Overwrite::Replace, Some(refuse_marked)).unwrap();
        fs::write(dest.join("marker"), b"").unwrap();
        assert!(
            out.publish().is_err(),
            "the validator is rerun before the swap"
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
        )
        .unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Collision { .. }));
        fs::write(&b, b"user data").unwrap();
        let err = OutputSet::files([&a, &b], Overwrite::Refuse).unwrap_err();
        assert!(matches!(output_error(&err), OutputError::Exists { .. }));
        assert!(!a.exists());
        assert!(others(dir.path(), &["b.osm.pbf"]).is_empty());
    }

    #[cfg(feature = "test-hooks")]
    mod faults {
        use super::*;

        fn staged_set(dir: &Path, overwrite: Overwrite) -> (OutputSet, PathBuf, PathBuf) {
            let a = dir.join("a.osm.pbf");
            let b = dir.join("b.osm.pbf");
            let set = OutputSet::files([&a, &b], overwrite).unwrap();
            for member in set.iter() {
                fs::write(member.path(), b"new").unwrap();
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
            let mut out = Output::new(&dest, Overwrite::Replace).unwrap();
            fs::write(out.path(), b"new").unwrap();
            out.inject_fault(PublishFault::BeforeInstall);
            assert!(out.publish().is_err());
            assert_eq!(fs::read(&dest).unwrap(), b"user data");
            assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());
        }

        #[test]
        fn the_link_fallback_publishes_exclusively() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            let mut out = Output::new(&dest, Overwrite::Refuse).unwrap();
            fs::write(out.path(), b"new").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            out.publish().unwrap();
            assert_eq!(fs::read(&dest).unwrap(), b"new");
            assert!(others(dir.path(), &["out.osm.pbf"]).is_empty());

            // Exclusive: an entry that appeared meanwhile is not replaced.
            let other = dir.path().join("other.osm.pbf");
            let mut out = Output::new(&other, Overwrite::Refuse).unwrap();
            fs::write(out.path(), b"ours").unwrap();
            fs::write(&other, b"theirs").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            let err = out.publish().unwrap_err();
            assert!(matches!(output_error(&err), OutputError::Exists { .. }));
            assert_eq!(fs::read(&other).unwrap(), b"theirs");
        }

        /// A failure after the link published the file rolls it back: the
        /// link committed it, so the rollback must remove it.
        #[test]
        fn a_failure_after_link_rolls_the_output_back() {
            let dir = scratch();
            let dest = dir.path().join("out.osm.pbf");
            let mut out = Output::new(&dest, Overwrite::Refuse).unwrap();
            fs::write(out.path(), b"new").unwrap();
            out.inject_fault(PublishFault::ForceLinkFallback);
            out.inject_fault(PublishFault::AfterLink);
            assert!(out.publish().is_err());
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
            assert!(set.publish().is_err());
            assert_eq!(fs::read(&a).unwrap(), b"old a");
            assert_eq!(fs::read(&b).unwrap(), b"old b");
            assert!(others(dir.path(), &["a.osm.pbf", "b.osm.pbf"]).is_empty());

            // Refuse: nothing existed, so a rolled-back group leaves nothing.
            let dir = scratch();
            let (mut set, a, b) = staged_set(dir.path(), Overwrite::Refuse);
            set.inject_fault(PublishFault::AfterInstalled(0));
            assert!(set.publish().is_err());
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
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None).unwrap();
            fs::write(out.path().join("new.bin"), b"new").unwrap();
            out.inject_fault(PublishFault::AfterBackup);
            assert!(out.publish().is_err());
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
            let err = set.publish().unwrap_err();
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
            let mut out = OutputDir::new(&dest, Overwrite::Replace, None).unwrap();
            fs::write(out.path().join("new.bin"), b"new").unwrap();
            out.inject_fault(PublishFault::AfterBackup);
            out.inject_fault(PublishFault::Restore);
            let err = out.publish().unwrap_err();
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
            let mut out = Output::new(&dest, Overwrite::Refuse).unwrap();
            fs::write(out.path(), b"new").unwrap();
            out.inject_fault(PublishFault::DirSync);
            let err = out.publish().unwrap_err();
            assert!(matches!(output_error(&err), OutputError::Unsynced { .. }));
            assert!(err.to_string().contains("was published"), "{err}");
            assert_eq!(fs::read(&dest).unwrap(), b"new");
        }
    }
}
