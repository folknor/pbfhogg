//! Canonical document and generated-role identities, independent of run paths.
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use super::describe::RoleKind;
use super::json::{self, Json};

/// A canonical document and its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Document {
    /// `plan-v1:<32 hex>` or `context-v1:<32 hex>`.
    pub(crate) digest: String,
    /// The document in brokkr-json-v1.
    pub(crate) canonical: String,
}

impl Document {
    /// `value` canonicalized, digested under `kind` (`plan` or `context`):
    /// XXH3-128 seed 0 of the canonical bytes, lowercase hex.
    pub(crate) fn of(kind: &str, value: &Json) -> Result<Self, String> {
        let canonical = json::canonical(value)?;
        let digest = format!(
            "{kind}-v1:{:032x}",
            xxhash_rust::xxh3::xxh3_128(canonical.as_bytes())
        );
        Ok(Self { digest, canonical })
    }
}

/// The identity of a generated role at `path` after the harness exited,
/// read without following a link: `absent`; for a file or PBF role
/// `xxh128:<hex>` of its bytes; for a directory role `tree-v1:<hex>`, the
/// XXH3-128 over its entries sorted by the raw bytes of their path relative
/// to it, each `<32 hex XXH3-128 of the path bytes><kind><value>\n`, kind `d`
/// (no value), `f` (the file's XXH128) or `l` (the XXH3-128 of the link's
/// raw target bytes), so every field has a fixed width. Any other entry, or
/// an entry not of the role's kind, cannot be identified.
pub(crate) fn generated(path: &Path, kind: RoleKind) -> Result<String, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("absent".to_owned()),
        Err(e) => return Err(format!("{}: cannot inspect: {e}", path.display())),
    };
    match kind {
        RoleKind::Pbf | RoleKind::File if meta.file_type().is_file() => {
            Ok(format!("xxh128:{}", file_digest(path)?))
        }
        RoleKind::Directory if meta.file_type().is_dir() => {
            let mut entries = Vec::new();
            walk(path, path, &mut entries)?;
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut hasher = xxhash_rust::xxh3::Xxh3::new();
            for (rel, record) in &entries {
                hasher.update(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(rel)).as_bytes());
                hasher.update(record.as_bytes());
                hasher.update(b"\n");
            }
            Ok(format!("tree-v1:{:032x}", hasher.digest128()))
        }
        _ => Err(format!(
            "{} is not a {}, so it cannot be identified",
            path.display(),
            kind.name()
        )),
    }
}

/// Every entry below `dir`, as (path bytes relative to `root`, kind and
/// value).
fn walk(root: &Path, dir: &Path, out: &mut Vec<(Vec<u8>, String)>) -> Result<(), String> {
    let read =
        std::fs::read_dir(dir).map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: cannot inspect: {e}", path.display()))?;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .as_os_str()
            .as_bytes()
            .to_vec();
        if kind.is_dir() {
            out.push((rel, "d".to_owned()));
            walk(root, &path, out)?;
        } else if kind.is_file() {
            out.push((rel, format!("f{}", file_digest(&path)?)));
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&path)
                .map_err(|e| format!("{}: cannot read the link: {e}", path.display()))?;
            out.push((
                rel,
                format!(
                    "l{:032x}",
                    xxhash_rust::xxh3::xxh3_128(target.as_os_str().as_bytes())
                ),
            ));
        } else {
            return Err(format!(
                "{} is neither a file, a directory nor a link, so it cannot be identified",
                path.display()
            ));
        }
    }
    Ok(())
}

/// XXH128 of a regular file, opened without following a link or blocking,
/// its type checked on the open descriptor.
pub(crate) fn file_digest(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let shown = path.display();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{shown}: cannot open: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{shown}: cannot inspect: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{shown}: not a regular file"));
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; crate::tunables::DIGEST_BUFFER];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("{shown}: cannot read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:032x}", hasher.digest128()))
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
