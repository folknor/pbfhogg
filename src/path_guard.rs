//! RAII guard for run-owned scratch paths, and the file-identity test.
//!
//! A [`PathGuard`] wraps a `PathBuf` and removes the pointed-at file or
//! directory when dropped, unless [`PathGuard::commit`] was called
//! first. Use it to avoid leaving scratch state on disk when a command
//! errors mid-stream. Outputs - anything a caller named - go through
//! [`crate::output`] instead.
//!
//! Happy-path cost is a single `Option::take`; the actual filesystem
//! work only runs on the error path.
//!
//! See `decisions/0003-error-path-hygiene-via-pathguard.md` for the
//! policy context: when to use this primitive, the "counters bump
//! after successful write" rule that accompanies it, and the
//! checklist for adding a new command.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Kind {
    File,
    Dir,
}

/// Guards a path the *run itself created*.
///
/// Never point this at a path that may already hold user data: Drop deletes
/// it unconditionally. A caller-named output is a [`crate::output::Output`],
/// which writes to a staging sibling and renames into place, so a mid-run
/// failure cannot touch a pre-existing file of that name.
#[doc(hidden)]
pub struct PathGuard {
    path: PathBuf,
    kind: Kind,
    armed: bool,
}

impl PathGuard {
    /// Guard a file path. On Drop without `commit()`, `remove_file` is
    /// attempted best-effort.
    pub fn file(path: PathBuf) -> Self {
        Self {
            path,
            kind: Kind::File,
            armed: true,
        }
    }

    /// Guard a directory path. On Drop without `commit()`,
    /// `remove_dir_all` is attempted best-effort.
    pub fn dir(path: PathBuf) -> Self {
        Self {
            path,
            kind: Kind::Dir,
            armed: true,
        }
    }

    /// Release the guard so the path survives Drop. Returns the path
    /// for convenience at the commit site (e.g. renaming into place).
    pub fn commit(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort. A failure here is less important than the
            // error the caller is about to return; swallowing avoids
            // masking that original error with a cleanup error.
            remove_best_effort(&self.path, self.kind);
        }
    }
}

/// The guard's one removal: of a path the run itself created, so it is not a
/// second scratch or output mechanism.
#[allow(clippy::disallowed_methods)]
fn remove_best_effort(path: &Path, kind: Kind) {
    match kind {
        Kind::File => drop(fs::remove_file(path)),
        Kind::Dir => drop(fs::remove_dir_all(path)),
    }
}

/// Whether two paths name the same existing file: equal after
/// canonicalization (which resolves symlinks), or on Unix the same device
/// and inode (hard links). A path that does not exist names no file, so it
/// aliases nothing and the answer is `false`; callers that also need to
/// catch a not-yet-created output resolving onto an input do that planning
/// themselves.
///
/// The one identity test behind every input/output alias refusal, in the
/// library entry points and the CLI preflight alike.
///
/// # Errors
/// Returns any canonicalization or metadata error other than not-found.
#[doc(hidden)]
pub fn same_file(a: &Path, b: &Path) -> std::io::Result<bool> {
    // Named: this runs on every input/output pair of a command line, and a
    // bare "Permission denied" does not say which of them it was.
    let named = |what: &str, path: &Path, e: std::io::Error| {
        std::io::Error::new(
            e.kind(),
            format!("failed to {what} {}: {e}", path.display()),
        )
    };
    // Any error but not-found fails the check rather than answering "not the
    // same file": resolving and stat-ing need the same directory search
    // permission the command's own open or staging create needs, so a path
    // this cannot resolve is one the run cannot use either, and failing here
    // is earlier and names the path.
    let canonical = |path: &Path| match fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(named("resolve", path, e)),
    };
    let Some(a) = canonical(a)? else {
        return Ok(false);
    };
    let Some(b) = canonical(b)? else {
        return Ok(false);
    };
    if a == b {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a_meta = fs::metadata(&a).map_err(|e| named("stat", &a, e))?;
        let b_meta = fs::metadata(&b).map_err(|e| named("stat", &b, e))?;
        Ok(a_meta.dev() == b_meta.dev() && a_meta.ino() == b_meta.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

#[cfg(test)]
#[path = "../tests/common/scratch.rs"]
mod shared_scratch;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Include only the shared allocator, keeping the integration harness
    // and its public-library imports out of the library under test.
    pub(crate) use super::shared_scratch::test_scratch as scratch;

    #[test]
    fn file_drop_removes() {
        let dir = scratch();
        let tmp = dir.path().join("guarded.tmp");
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let _g = PathGuard::file(tmp.clone());
        }
        assert!(!tmp.exists(), "file should be removed on drop");
    }

    #[test]
    fn file_commit_preserves() {
        let dir = scratch();
        let tmp = dir.path().join("committed.tmp");
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let g = PathGuard::file(tmp.clone());
            let returned = g.commit();
            assert_eq!(returned, tmp);
        }
        assert!(tmp.exists(), "file should survive after commit");
    }

    #[test]
    fn dir_drop_removes_recursively() {
        let dir = scratch();
        let tmp = dir.path().join("guarded-dir");
        fs::create_dir_all(&tmp).expect("create test dir");
        fs::write(tmp.join("inside.txt"), b"x").expect("write test file");
        {
            let _g = PathGuard::dir(tmp.clone());
        }
        assert!(!tmp.exists(), "dir should be removed recursively on drop");
    }

    #[cfg(unix)]
    #[test]
    fn same_file_recognizes_symlinks_and_hard_links() {
        let dir = scratch();
        let base = dir.path().join("base.osm.pbf");
        let hard = dir.path().join("hard.osm.pbf");
        let symlink = dir.path().join("symlink.osm.pbf");
        let other = dir.path().join("other.osm.pbf");
        let absent = dir.path().join("absent.osm.pbf");
        fs::write(&base, b"base").expect("write base");
        fs::write(&other, b"base").expect("write other");
        fs::hard_link(&base, &hard).expect("create hard link");
        std::os::unix::fs::symlink(&base, &symlink).expect("create symlink");

        assert!(same_file(&base, &base).expect("identity"));
        assert!(same_file(&base, &hard).expect("hard link identity"));
        assert!(same_file(&symlink, &base).expect("symlink identity"));
        assert!(!same_file(&base, &other).expect("equal bytes, different file"));
        assert!(!same_file(&base, &absent).expect("absent output"));
        assert!(!same_file(&absent, &base).expect("absent input"));
    }
}
