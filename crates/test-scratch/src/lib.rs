//! The one owner of test scratch for every pbfhogg test harness: the
//! library's unit and integration tests, the CLI's and the library
//! harness's. Dev-only; nothing shipped depends on it. It also holds the
//! other support every harness shares and that needs nothing from pbfhogg:
//! the repository root and the fail-stop hang bound ([`HangBound`]).
//!
//! Every scratch directory is a random child of the workspace's gitignored
//! `data/test-scratch`. Random names cannot collide between tests, sweeps or
//! processes, so no slot, claim registry or sweeper is needed, and a test
//! binary never deletes anything it did not create. Residue is cleared at
//! the run boundary instead: `scripts/test_tree.py`, run by `brokkr check`
//! before its test phase, empties the base, and after a green test phase
//! requires it to be empty again. A binary cannot tell a live sibling's tree
//! from residue, so it must not sweep.
//!
//! The location is fixed relative to this crate: the workspace is two levels
//! above `crates/test-scratch`. It is not relocatable; a test binary run from
//! a moved or read-only checkout still names the tree it was compiled in.

mod hang;
pub use hang::HangBound;

use std::fs::{File, FileType};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The repository root: the workspace two levels above this crate's
/// manifest, checked once per process to hold a `Cargo.toml` with a
/// `[workspace]` table. That check is a layout sanity check (the crate has
/// not moved deeper or shallower), not proof that the directory is this
/// repository.
///
/// For source-contract checks that read the checkout (documentation, the
/// brokkr config). It creates nothing, so reading the tree never needs a
/// writable `data/`.
///
/// # Panics
///
/// If the workspace manifest cannot be read or has no `[workspace]` line.
pub fn repository_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap_or_else(|error| {
            panic!("read the workspace manifest {}: {error}", manifest.display())
        });
        assert!(
            text.lines().any(|line| line.trim() == "[workspace]"),
            "{} has no [workspace] table: crates/test-scratch is no longer two levels below the workspace root",
            manifest.display()
        );
        root
    })
}

/// The scratch base, `<repository root>/data/test-scratch`, created once per
/// process.
///
/// The root is deliberately fixed and has no environment override: the test
/// runner exports no scratch-root variable, so a variable read here would
/// be one nothing sets, and a second location would split residue between
/// two trees. Do not add one.
fn scratch_base() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let base = repository_root().join("data/test-scratch");
        std::fs::create_dir_all(&base).unwrap_or_else(|error| {
            panic!("create the test scratch base {}: {error}", base.display())
        });
        base
    })
}

/// A fresh scratch directory. Keep the guard alive for the fixture's
/// lifetime; see [`TestScratch`] for what dropping it does.
///
/// # Panics
///
/// If the scratch base or the directory cannot be created.
#[must_use]
pub fn scratch() -> TestScratch {
    let base = scratch_base();
    let dir = tempfile::TempDir::new_in(base).unwrap_or_else(|error| {
        panic!(
            "create a test scratch directory in {}: {error}",
            base.display()
        )
    });
    TestScratch {
        dir: Some(dir),
        next_cell: AtomicUsize::new(0),
    }
}

/// A test's own scratch directory.
///
/// Dropping it:
///
/// - **during unwinding** keeps the tree, so the test that panicked with the
///   guard in scope leaves its fixture for inspection. A guard destroyed
///   during unwinding keeps its tree; everything else is removed. Unwinding
///   is not a test verdict: a guard created inside a closure whose panic a
///   passing test catches is kept too, and a property that fails through a
///   returned `prop_assert!` failure loses its case's tree.
/// - **otherwise** removes the tree and panics, naming the path, if removal
///   fails, so a test that leaves an unremovable tree (a permission test that
///   did not restore a mode) fails instead of leaking it. A failed removal
///   may already have deleted part of the tree, so what remains is partial,
///   not a preserved fixture.
///
/// That panic fails the test only when the guard is dropped on the test's
/// own thread, or on a worker whose failure the test propagates. Keep guards
/// as ordinary scoped owners: a guard in a `static` is never dropped, and one
/// in thread-local storage aborts the process if its removal panics. Keep a
/// guard outside any closure whose panic a test expects, or a removal panic
/// could pass for the expected one.
///
/// Residue in `data/test-scratch` therefore grows on unwinding, on
/// [`keep`](Self::keep), on cleanup failure and on process termination.
/// Nothing in a test binary clears it; `brokkr check` empties the base
/// before its test phase and fails a green test phase that leaves any tree
/// behind, so a passing test that keeps a tree (by `keep` or a caught
/// unwind) removes it itself before it returns.
#[derive(Debug)]
pub struct TestScratch {
    /// Taken exactly once, by `keep` or by `drop`, so no second cleanup
    /// attempt is ever left armed.
    dir: Option<tempfile::TempDir>,
    next_cell: AtomicUsize,
}

impl TestScratch {
    /// The directory.
    ///
    /// # Panics
    ///
    /// Never: the directory is only taken by `keep` (which consumes the
    /// guard) or by `drop`.
    #[must_use]
    pub fn path(&self) -> &Path {
        match &self.dir {
            Some(dir) => dir.path(),
            None => unreachable!("a TestScratch's directory is taken only when it is consumed"),
        }
    }

    /// A single normal component below this directory; creates nothing.
    /// Containment is lexical only: an existing symlink can still escape.
    ///
    /// # Panics
    /// If the name is not exactly one normal path component.
    pub fn child(&self, name: impl AsRef<Path>) -> PathBuf {
        let name = name.as_ref();
        let mut parts = name.components();
        assert!(
            matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none(),
            "scratch child must be one normal component: {}",
            name.display()
        );
        // Components normalizes trailing separators and dot segments.
        assert!(
            name.file_name()
                .is_some_and(|part| name.as_os_str() == part),
            "scratch child must be one normal component: {}",
            name.display()
        );
        self.path().join(name)
    }

    /// Create the next numbered directory. Existing entries are never reused.
    ///
    /// # Panics
    /// On counter exhaustion or directory creation failure.
    pub fn cell(&self) -> PathBuf {
        let number = self
            .next_cell
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .unwrap_or_else(|_| panic!("scratch cell counter exhausted"));
        let path = self.child(number.to_string());
        std::fs::create_dir(&path)
            .unwrap_or_else(|e| panic!("create scratch cell {}: {e}", path.display()));
        path
    }

    /// Inspect without following a symlink. Only NotFound means absent.
    pub fn path_state(&self, path: impl AsRef<Path>) -> io::Result<Option<FileType>> {
        path_state(path)
    }

    /// Create an anonymous file inside this scratch.
    ///
    /// # Panics
    /// If creating the file fails.
    pub fn anonymous_file(&self) -> File {
        tempfile::tempfile_in(self.path())
            .unwrap_or_else(|e| panic!("create anonymous scratch file: {e}"))
    }

    /// Deny creation of entries in a directory, restoring its original mode
    /// through an open descriptor even if it is renamed. This proves nothing
    /// about removing or renaming entries, or opening existing files.
    /// Returns an unavailable reason if mode bits do not enforce the denial.
    ///
    /// # Panics
    /// On unexpected setup, inspection, probe or restoration errors.
    #[cfg(unix)]
    pub fn deny_write(&self, path: impl AsRef<Path>) -> Result<DenyWrite<'_>, String> {
        use std::os::unix::fs::PermissionsExt;
        let path = path.as_ref();
        assert!(
            path.starts_with(self.path()),
            "permission fixture must be inside scratch"
        );
        let directory =
            File::open(path).unwrap_or_else(|e| panic!("open directory {}: {e}", path.display()));
        let meta = directory
            .metadata()
            .unwrap_or_else(|e| panic!("inspect directory: {e}"));
        assert!(meta.is_dir(), "permission fixture must be a directory");
        let original = meta.permissions();
        let guard = DenyWrite {
            directory,
            original,
            path: path.to_path_buf(),
            _scratch: self,
        };
        guard
            .directory
            .set_permissions(std::fs::Permissions::from_mode(
                meta.permissions().mode() & !0o222,
            ))
            .unwrap_or_else(|e| panic!("deny directory writes: {e}"));
        match tempfile::NamedTempFile::new_in(path) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(guard),
            Ok(probe) => {
                probe
                    .close()
                    .unwrap_or_else(|e| panic!("remove permission probe: {e}"));
                Err(format!(
                    "creating an entry in {} still succeeds with write bits removed",
                    path.display()
                ))
            }
            Err(e) => panic!("probe directory write denial {}: {e}", path.display()),
        }
    }

    /// Keep the tree as deliberate evidence and return its path. Nothing
    /// removes it afterwards.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        match self.dir.take() {
            Some(dir) => dir.keep(),
            None => unreachable!("a TestScratch's directory is taken only when it is consumed"),
        }
    }
}

/// Inspect without following symlinks; only NotFound means absence.
pub fn path_state(path: impl AsRef<Path>) -> io::Result<Option<FileType>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta.file_type())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Restores the original directory mode before the borrowed scratch is removed.
#[cfg(unix)]
#[derive(Debug)]
pub struct DenyWrite<'a> {
    directory: File,
    original: std::fs::Permissions,
    path: PathBuf,
    _scratch: &'a TestScratch,
}

#[cfg(unix)]
impl Drop for DenyWrite<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.directory.set_permissions(self.original.clone()) {
            if std::thread::panicking() {
                eprintln!("restore directory permissions {}: {e}", self.path.display());
            } else {
                panic!("restore directory permissions {}: {e}", self.path.display());
            }
        }
    }
}

impl Drop for TestScratch {
    fn drop(&mut self) {
        let Some(dir) = self.dir.take() else {
            return;
        };
        if std::thread::panicking() {
            let _kept = dir.keep();
            return;
        }
        let path = dir.path().to_path_buf();
        if let Err(error) = dir.close() {
            panic!(
                "failed to remove test scratch tree {}: {error}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{repository_root, scratch};

    #[test]
    fn child_is_lexical_and_creates_nothing() {
        let dir = scratch();
        assert!(dir.path_state(dir.child("file")).expect("state").is_none());
        for name in ["", ".", "..", "a/b", "/absolute", "a/..", "a/"] {
            assert!(
                std::panic::catch_unwind(|| dir.child(name)).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn cells_are_distinct_and_never_reused() {
        let dir = scratch();
        assert_eq!(dir.cell(), dir.child("0"));
        assert_eq!(dir.cell(), dir.child("1"));
        std::fs::write(dir.child("2"), b"occupied").expect("write");
        assert!(std::panic::catch_unwind(|| dir.cell()).is_err());
        assert_eq!(dir.cell(), dir.child("3"));
    }

    #[test]
    fn state_preserves_symlinks_and_errors() {
        let dir = scratch();
        let file = dir.child("file");
        std::fs::write(&file, b"x").expect("write");
        assert!(
            dir.path_state(&file)
                .expect("state")
                .expect("file")
                .is_file()
        );
        assert!(dir.path_state(file.join("child")).is_err());
        #[cfg(unix)]
        {
            let link = dir.child("link");
            std::os::unix::fs::symlink(dir.child("missing"), &link).expect("symlink");
            assert!(
                dir.path_state(link)
                    .expect("state")
                    .expect("link")
                    .is_symlink()
            );
        }
    }

    #[test]
    fn anonymous_file_roundtrips() {
        use std::io::{Read, Seek, Write};
        let dir = scratch();
        let mut file = dir.anonymous_file();
        file.write_all(b"anonymous").expect("write");
        file.rewind().expect("rewind");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read");
        assert_eq!(bytes, b"anonymous");
    }

    #[cfg(unix)]
    #[test]
    fn denial_restores_the_renamed_directory_during_unwinding() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch();
        let original = dir.cell();
        let mode = std::fs::metadata(&original)
            .expect("metadata")
            .permissions()
            .mode();
        let renamed = dir.child("renamed");
        let denial = match dir.deny_write(&original) {
            Ok(guard) => guard,
            Err(reason) => {
                eprintln!("SKIP: {reason}");
                assert_eq!(
                    std::fs::metadata(&original)
                        .expect("metadata")
                        .permissions()
                        .mode(),
                    mode
                );
                return;
            }
        };
        assert_eq!(
            std::fs::write(original.join("no"), b"x")
                .expect_err("denied")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _denial = denial;
            std::fs::rename(&original, &renamed).expect("rename");
            panic!("exercise restoration on unwind");
        }));
        assert!(caught.is_err());
        assert_eq!(
            std::fs::metadata(&renamed)
                .expect("metadata")
                .permissions()
                .mode(),
            mode
        );
        std::fs::write(renamed.join("yes"), b"x").expect("restored");
    }

    #[test]
    fn a_clean_drop_removes_the_tree() {
        let dir = scratch();
        let path = dir.path().to_path_buf();
        std::fs::write(path.join("f"), b"x").expect("write");
        drop(dir);
        assert_eq!(
            std::fs::symlink_metadata(&path).map_err(|e| e.kind()).err(),
            Some(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_guard_dropped_while_unwinding_keeps_its_tree() {
        let (tx, rx) = std::sync::mpsc::channel();
        let unwound = std::panic::catch_unwind(move || {
            let dir = scratch();
            tx.send(dir.path().to_path_buf()).expect("send");
            panic!("unwind with the guard in scope");
        });
        assert!(unwound.is_err());
        let path = rx.recv().expect("path");
        assert!(std::fs::symlink_metadata(&path).expect("kept").is_dir());
        std::fs::remove_dir_all(&path).expect("tidy the kept tree");
    }

    #[test]
    fn keep_leaves_the_tree() {
        let path = scratch().keep();
        assert!(std::fs::symlink_metadata(&path).expect("kept").is_dir());
        std::fs::remove_dir_all(&path).expect("tidy the kept tree");
    }

    #[test]
    fn scratch_lives_under_the_repository_root() {
        let dir = scratch();
        assert!(
            dir.path()
                .starts_with(repository_root().join("data/test-scratch"))
        );
        assert!(
            repository_root()
                .join("crates/test-scratch/Cargo.toml")
                .is_file()
        );
    }
}
