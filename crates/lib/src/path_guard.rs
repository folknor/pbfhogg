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

/// `(st_dev, st_ino)`.
type Identity = (u64, u64);

fn identity_of(meta: &fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// What removing a directory tree recursively would take with it, as seen
/// from one file: the identity of every directory in the tree (the root
/// included), and every entry, as (parent directory identity, name), that is
/// a hard link to the file. Symlinks are never followed, so a link inside
/// pointing out costs only the link. Identities rather than paths, so a bind
/// mount aliasing the tree or a directory in it is still recognised, and a
/// directory reached twice is walked once and its entries counted once.
#[derive(Default)]
struct Removed {
    dirs: std::collections::HashSet<Identity>,
    links: std::collections::HashSet<(Identity, std::ffi::OsString)>,
}

fn removed_with(dir: &Path, file: Identity) -> std::io::Result<Removed> {
    let mut removed = Removed::default();
    let root = match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => meta,
        Ok(_) => return Ok(removed),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    let root = identity_of(&root);
    removed.dirs.insert(root);
    let mut pending = vec![(dir.to_path_buf(), root)];
    while let Some((next, parent)) = pending.pop() {
        for entry in fs::read_dir(&next)? {
            let entry = entry?;
            // `DirEntry::metadata` does not follow a symlink.
            let meta = entry.metadata()?;
            let id = identity_of(&meta);
            if meta.is_dir() {
                if removed.dirs.insert(id) {
                    pending.push((entry.path(), id));
                }
            } else if id == file {
                removed.links.insert((parent, entry.file_name()));
            }
        }
    }
    Ok(removed)
}

/// Whether replacing the directory `dir` wholesale (moved aside, then
/// removed recursively) would delete the run's input: `file` is the input
/// file's live metadata and `label` the path the caller named it by.
///
/// Refused when the tree holds every remaining link to the file (an outside
/// hard link survives, so then the file does too), or, while `label` still
/// resolves to the file, when the entry `label` names, or the file it
/// resolves to, sits in a directory of the tree (a symlink inside is
/// deleted with it even when its target is not). A path that merely passes
/// through a symlinked directory inside `dir` loses only that link and is
/// allowed. A file with no link at all (an unlinked, pinned input) has
/// nothing to lose. A label that no longer names the file is not judged by
/// name; the link rule alone covers a file moved after it was named.
///
/// The judgement is of the tree as it is now; a caller rechecks immediately
/// before the replacement, and namespace changes after that are outside it.
///
/// # Errors
/// Any error walking `dir` or resolving `label` other than not-found: what
/// the walk did not see cannot be shown to survive.
pub(crate) fn replacing_dir_deletes(
    dir: &Path,
    label: &Path,
    file: &fs::Metadata,
) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let id = identity_of(file);
    let removed = removed_with(dir, id)?;
    if removed.dirs.is_empty() {
        return Ok(false);
    }
    if !removed.links.is_empty() && removed.links.len() as u64 >= file.nlink() {
        return Ok(true);
    }
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    match fs::metadata(label) {
        Ok(meta) if identity_of(&meta) == id => {}
        Ok(_) => return Ok(false),
        Err(e) if not_found(&e) => return Ok(false),
        Err(e) => return Err(e),
    }
    let inside = |path: &Path| match fs::metadata(path) {
        Ok(meta) => Ok(removed.dirs.contains(&identity_of(&meta))),
        Err(e) if not_found(&e) => Ok(false),
        Err(e) => Err(e),
    };
    // A bare relative file name's parent is the current directory.
    let entry_parent = label
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if inside(entry_parent)? {
        return Ok(true);
    }
    match fs::canonicalize(label) {
        Ok(target) => match target.parent() {
            Some(parent) => inside(parent),
            None => Ok(false),
        },
        Err(e) if not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether a directory tree holds any link to `file` at all, whatever other
/// links exist: removing a tree that holds one is never done on the strength
/// of a link count that may have gone stale.
///
/// # Errors
/// Any error walking `dir`.
pub(crate) fn tree_links_to(dir: &Path, file: &fs::Metadata) -> std::io::Result<bool> {
    Ok(!removed_with(dir, identity_of(file))?.links.is_empty())
}

/// Whether replacing the directory `dir` would delete `input`, by the rule
/// of [`replacing_dir_deletes`]: the CLI's preflight, which refuses before
/// any work what the library refuses again before publication. A missing
/// input or directory holds nothing; the command's own checks say what is
/// wrong with it.
///
/// # Errors
/// Any error but not-found, naming the path.
#[doc(hidden)]
pub fn dir_holds(dir: &Path, input: &Path) -> std::io::Result<bool> {
    let named = |path: &Path, e: std::io::Error| {
        std::io::Error::new(
            e.kind(),
            format!(
                "failed to check whether {} holds {}: {e}",
                dir.display(),
                path.display()
            ),
        )
    };
    let file = match fs::metadata(input) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(named(input, e)),
    };
    replacing_dir_deletes(dir, input, &file).map_err(|e| named(input, e))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // The library's unit tests take scratch from the dev-only
    // `pbfhogg-test-scratch` crate, the same owner every harness uses.
    pub(crate) use pbfhogg_test_scratch::scratch;

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

    /// Replacing a directory deletes an input inside it, and the entry that
    /// names an input even when that entry is a symlink to a file outside.
    /// A path that only passes through the directory, or through a
    /// symlinked directory inside it, loses nothing the run reads, and a
    /// file with a hard link outside survives.
    #[test]
    fn a_directory_holds_an_input_inside_it_or_the_entry_naming_it() {
        let root = scratch();
        let idx = root.path().join("idx");
        let outside = root.path().join("outside");
        fs::create_dir(&idx).expect("fixture");
        fs::create_dir(&outside).expect("fixture");
        let source = outside.join("in.osm.pbf");
        fs::write(&source, b"pbf").expect("fixture");
        let inside = idx.join("in.osm.pbf");
        fs::write(&inside, b"pbf").expect("fixture");
        let link_in = idx.join("link.osm.pbf");
        std::os::unix::fs::symlink(&source, &link_in).expect("fixture");
        let link_out = outside.join("into-idx.osm.pbf");
        std::os::unix::fs::symlink(&inside, &link_out).expect("fixture");
        let dir_link = idx.join("elsewhere");
        std::os::unix::fs::symlink(&outside, &dir_link).expect("fixture");
        let buckets = idx.join(".buckets-level9");
        fs::create_dir(&buckets).expect("fixture");
        let hard = buckets.join("hard.osm.pbf");
        fs::hard_link(&source, &hard).expect("fixture");

        assert!(dir_holds(&idx, &inside).expect("fixture"));
        assert!(dir_holds(&idx, &link_in).expect("fixture"));
        assert!(dir_holds(&idx, &link_out).expect("fixture"));
        assert!(!dir_holds(&idx, &source).expect("fixture"));
        assert!(
            !dir_holds(&idx, &idx.join("..").join("outside").join("in.osm.pbf")).expect("fixture")
        );
        assert!(!dir_holds(&idx, &dir_link.join("in.osm.pbf")).expect("fixture"));
        assert!(!dir_holds(&idx, &outside.join("missing.osm.pbf")).expect("fixture"));
        assert!(!dir_holds(&root.path().join("no-such-index"), &source).expect("fixture"));
        // The name given is inside, though another link survives outside.
        assert!(dir_holds(&idx, &hard).expect("fixture"));
        assert!(tree_links_to(&idx, &fs::metadata(&source).expect("fixture")).expect("fixture"));

        // The outside name goes: the link inside is now the last one, and a
        // label that no longer names the file is judged by links alone.
        fs::remove_file(&source).expect("fixture");
        let file = fs::metadata(&hard).expect("fixture");
        assert!(replacing_dir_deletes(&idx, &source, &file).expect("fixture"));
    }
}
