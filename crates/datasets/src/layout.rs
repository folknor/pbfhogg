//! File names of downloaded and generated inputs, and their publication.
//!
//! A file only ever appears at its final name by a rename of a staged
//! sibling after its writer exited successfully, followed by a directory
//! fsync. Under that discipline a file present at a final name is complete.

use std::path::{Path, PathBuf};

pub(crate) fn osc_filename(key: &str, seq: u64) -> String {
    format!("{key}-seq{seq}.osc.gz")
}

pub(crate) fn raw_pbf_filename(key: &str, date_or_snapshot: &str) -> String {
    format!("{key}-{date_or_snapshot}.osm.pbf")
}

pub(crate) fn indexed_pbf_filename(key: &str, date_or_snapshot: &str) -> String {
    format!("{key}-{date_or_snapshot}-with-indexdata.osm.pbf")
}

/// The staged sibling a writer fills before publication. It keeps the final
/// name's suffix (pbfhogg reads the output format from it) and is hidden.
pub(crate) fn partial(dest: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".partial-");
    name.push(dest.file_name().unwrap_or_default());
    dest.with_file_name(name)
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
/// writer of it can be alive.
pub(crate) fn clear(staged: &Path) -> Result<(), String> {
    match std::fs::remove_file(staged) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {e}", staged.display()))
        }
        _ => Ok(()),
    }
}

/// Resolve symlinks even when the final filename does not exist yet.
pub(crate) fn publication_path(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("file has no parent")?;
            Ok(publication_path(parent)?.join(path.file_name().ok_or("file has no name")?))
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
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
            partial(Path::new("data/d-1.osm.pbf")),
            Path::new("data/.partial-d-1.osm.pbf")
        );
    }
}
