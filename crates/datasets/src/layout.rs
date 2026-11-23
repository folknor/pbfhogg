//! File names of downloaded and generated inputs, and their publication.
//!
//! A file only ever appears at its final name by a rename of a staged
//! sibling after its writer exited successfully, followed by a directory
//! fsync. Under that discipline a file present at a final name is complete.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use pbfhogg_devkit::Config;

pub(crate) fn osc_filename(key: &str, seq: u64) -> String {
    format!("{key}-seq{seq}.osc.gz")
}

pub(crate) fn raw_pbf_filename(key: &str, date_or_snapshot: &str) -> String {
    format!("{key}-{date_or_snapshot}.osm.pbf")
}

pub(crate) fn indexed_pbf_filename(key: &str, date_or_snapshot: &str) -> String {
    format!("{key}-{date_or_snapshot}-with-indexdata.osm.pbf")
}

/// The staged sibling a writer for dataset `key` fills before publication.
/// It keeps the final name's suffix (pbfhogg reads the output format from
/// it) and is hidden. The key is delimited by `.`, which a dataset key never
/// contains, so two datasets - two different dataset locks - never share a
/// staging name even where their final names would coincide (`foo-bar`
/// snapshot `baz` and `foo` snapshot `bar-baz`).
pub(crate) fn partial(key: &str, dest: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(format!(".partial.{key}."));
    name.push(dest.file_name().unwrap_or_default());
    dest.with_file_name(name)
}

/// Every file registered on the host: (dataset key, path joined to `dir`).
pub(crate) fn registered_paths(
    config: &Config,
    host: &str,
    dir: &Path,
) -> Result<Vec<(String, PathBuf)>, String> {
    Ok(config
        .registered_files(host)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(key, file)| (key, dir.join(file)))
        .collect())
}

/// Refuse when replacing, filling or unlinking the directory entry `target`
/// would break a registered file anywhere on the host, or any path in
/// `protect`; `role` names the entry in the refusal.
///
/// The question is whether resolving a protected path looks up the very
/// directory entry the operation changes, not whether the two paths end at
/// the same file: a registration can traverse the name as a symlinked
/// directory or reach it through a dangling symlink, and an unused symlink
/// at the name that points at a registered file is harmless to replace.
/// Directories are compared by device and inode, so a bind-mounted alias of
/// the data directory is seen through. Names compare bytewise, so the
/// guarantee holds on case-sensitive directories only; `..` is popped
/// lexically, which can over-refuse but never under-refuse.
fn veto(
    config: &Config,
    host: &str,
    dir: &Path,
    target: &Path,
    role: &str,
    protect: &[&Path],
) -> Result<(), String> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let meta = std::fs::metadata(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let entry = (
        (meta.dev(), meta.ino()),
        target.file_name().unwrap_or_default(),
    );
    for path in protect {
        if resolution_uses(path, entry)? {
            return Err(format!(
                "{} is the {role}, but resolving {} passes through it; move it elsewhere first",
                target.display(),
                path.display()
            ));
        }
    }
    for (owner, path) in registered_paths(config, host, dir)? {
        if resolution_uses(&path, entry)? {
            return Err(format!(
                "{} is the {role}, but resolving {} registered by '{owner}' passes \
                 through it; it is already registered",
                target.display(),
                path.display()
            ));
        }
    }
    Ok(())
}

/// Refuse to write, replace or adopt the final name `dest` when any
/// registration on the host resolves through that directory entry (see
/// `veto`). Every producer asks this before touching a final name.
pub(crate) fn claim(config: &Config, host: &str, dir: &Path, dest: &Path) -> Result<(), String> {
    veto(config, host, dir, dest, "publication name", &[])
}

/// Refuse to create or open the dataset lock at `lock` when any registration
/// on the host, or any path in `protect`, resolves through that directory
/// entry (see `veto`): its creation would fill an absent registered name.
pub(crate) fn reserve_lock(
    config: &Config,
    host: &str,
    dir: &Path,
    lock: &Path,
    protect: &[&Path],
) -> Result<(), String> {
    veto(config, host, dir, lock, "dataset lock name", protect)
}

/// Prepare the staged sibling of `dest` for a writer of dataset `key`:
/// refuse when unlinking it would break a registered file anywhere on the
/// host, or any path in `protect` (see `veto`), else remove whatever stale
/// file is there.
pub(crate) fn stage(
    config: &Config,
    host: &str,
    dir: &Path,
    key: &str,
    dest: &Path,
    protect: &[&Path],
) -> Result<PathBuf, String> {
    let staged = partial(key, dest);
    veto(config, host, dir, &staged, "staging name", protect)?;
    clear(&staged)?;
    Ok(staged)
}

/// Make a file found complete at its final name durable before it is
/// registered: a run that died between rename and directory fsync left a
/// name a power failure could still lose.
pub(crate) fn make_durable(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    sync_parent(path)
}

/// Whether resolving `path` looks up `name` inside the directory identified
/// by `(dev, ino)`. Symlinks are expanded in place, as the kernel would.
fn resolution_uses(path: &Path, (parent, name): ((u64, u64), &OsStr)) -> Result<bool, String> {
    let absolute = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut pending: VecDeque<OsString> = components(&absolute);
    let mut cur = PathBuf::from("/");
    let mut hops = 0;
    while let Some(comp) = pending.pop_front() {
        if comp == "/" {
            cur = PathBuf::from("/");
            continue;
        }
        if comp == "." {
            continue;
        }
        if comp == ".." {
            cur.pop();
            continue;
        }
        if comp == name && std::fs::metadata(&cur).is_ok_and(|m| (m.dev(), m.ino()) == parent) {
            return Ok(true);
        }
        let candidate = cur.join(&comp);
        match std::fs::symlink_metadata(&candidate) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > SYMLINK_HOPS {
                    return Err(format!(
                        "{}: too many levels of symbolic links",
                        path.display()
                    ));
                }
                let target = std::fs::read_link(&candidate)
                    .map_err(|e| format!("{}: {e}", candidate.display()))?;
                for c in components(&target).into_iter().rev() {
                    pending.push_front(c);
                }
            }
            Ok(_) => cur = candidate,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => cur = candidate,
            Err(e) => return Err(format!("{}: {e}", candidate.display())),
        }
    }
    Ok(false)
}

/// Linux's limit on symlinks followed in one resolution (`MAXSYMLINKS`).
const SYMLINK_HOPS: usize = 40;

fn components(path: &Path) -> VecDeque<OsString> {
    path.components()
        .map(|c| match c {
            Component::RootDir | Component::Prefix(_) => OsString::from("/"),
            Component::CurDir => OsString::from("."),
            Component::ParentDir => OsString::from(".."),
            Component::Normal(n) => n.to_owned(),
        })
        .collect()
}

/// fsync the directory containing `path`.
pub(crate) fn sync_parent(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Publish a finished staged file: fsync it, rename it to `dest`, fsync the
/// directory, so a registry entry committed afterwards never names a file a
/// crash could lose.
pub(crate) fn publish(staged: &Path, dest: &Path) -> Result<(), String> {
    std::fs::File::open(staged)
        .and_then(|f| f.sync_all())
        .and_then(|()| std::fs::rename(staged, dest))
        .map_err(|e| format!("{} -> {}: {e}", staged.display(), dest.display()))?;
    sync_parent(dest)
}

/// Remove a stale staged file. Only called under the dataset lock, when no
/// writer of it can be alive: the staging name carries the dataset key, and
/// every writer of it holds that key's lock.
fn clear(staged: &Path) -> Result<(), String> {
    match std::fs::remove_file(staged) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", staged.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn names_follow_the_registry_conventions() {
        assert_eq!(
            raw_pbf_filename("denmark", "20260411"),
            "denmark-20260411.osm.pbf"
        );
        assert_eq!(
            indexed_pbf_filename("denmark", "20260411"),
            "denmark-20260411-with-indexdata.osm.pbf"
        );
        assert_eq!(
            partial("d", Path::new("data/d-1.osm.pbf")),
            Path::new("data/.partial.d.d-1.osm.pbf")
        );
    }

    #[test]
    fn resolution_sees_the_staging_entry_through_symlinks_and_mount_spellings() {
        use std::os::unix::fs::symlink;
        let scratch = pbfhogg_test_scratch::scratch();
        let root = std::fs::canonicalize(scratch.path()).unwrap();
        let data = root.join("data");
        std::fs::create_dir_all(data.join("store")).unwrap();
        std::fs::write(data.join("store/raw.osm.pbf"), b"x").unwrap();
        let staged = data.join(".partial.d.new.osm.pbf");
        let meta = std::fs::metadata(&data).unwrap();
        let entry = ((meta.dev(), meta.ino()), staged.file_name().unwrap());

        // A registration that traverses the staging name as a directory.
        symlink(data.join("store"), &staged).unwrap();
        let through = data.join(".partial.d.new.osm.pbf/raw.osm.pbf");
        assert!(resolution_uses(&through, entry).unwrap());
        // The same, reached through a symlinked alias of the data directory
        // and a relative link target.
        symlink(&data, root.join("alias")).unwrap();
        symlink(".partial.d.new.osm.pbf/raw.osm.pbf", data.join("indirect")).unwrap();
        assert!(resolution_uses(&root.join("alias/indirect"), entry).unwrap());
        assert!(
            resolution_uses(&root.join("alias/store/../.partial.d.new.osm.pbf"), entry).unwrap()
        );
        // A registration of the target file itself does not use the entry.
        assert!(!resolution_uses(&data.join("store/raw.osm.pbf"), entry).unwrap());
        assert!(!resolution_uses(&data.join("absent/x"), entry).unwrap());
        // A dangling alias of an absent final name is a use of that entry.
        let fresh = data.join("fresh.osm.pbf");
        let fresh_entry = ((meta.dev(), meta.ino()), fresh.file_name().unwrap());
        symlink(&fresh, root.join("dangling")).unwrap();
        assert!(resolution_uses(&root.join("dangling"), fresh_entry).unwrap());
    }
}
