use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{Config, Error};

/// Exact role binding and content identity. Unpinned inputs still have an
/// observed digest; a configured pin is never substituted for observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInput {
    pub role: String,
    pub variant: String,
    pub path: PathBuf,
    pub expected_digest: Option<String>,
    pub observed_digest: String,
}

/// Compute XXH3-128 over file bytes, printed as 32 lowercase hex digits.
/// Directories and unreadable inputs are errors. No cache can hide changes.
pub fn file_digest(path: &Path) -> Result<String, Error> {
    let at = |e| Error::message(format!("{}: {e}", path.display()));
    let mut file = std::fs::File::open(path).map_err(at)?;
    if !file.metadata().map_err(at)?.is_file() {
        return Err(Error::message(format!(
            "{}: expected a regular file",
            path.display()
        )));
    }
    let before = stamp(&file.metadata().map_err(at)?);
    let mut hash = xxhash_rust::xxh3::Xxh3::new();
    let mut buffer = vec![0; crate::tunables::DIGEST_BUFFER_BYTES];
    loop {
        let count = match file.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other.map_err(at)?,
        };
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = stamp(&file.metadata().map_err(at)?);
    let named = stamp(&std::fs::metadata(path).map_err(at)?);
    if before != after || after != named {
        return Err(Error::message(format!(
            "{}: changed while it was hashed",
            path.display()
        )));
    }
    Ok(format!("{:032x}", hash.digest128()))
}

impl Config {
    /// Resolve a PBF role from the selected host, dataset, snapshot and
    /// variant. None and "base" select primary data. Named snapshots never
    /// fall back to primary data. Relative file paths use the host's data
    /// directory (default "data"), itself relative to the project root.
    pub fn resolve_input(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
        role: &str,
        variant: &str,
    ) -> Result<ResolvedInput, Error> {
        let host_table = self.selected_host(host)?;
        let datasets = child_table(host_table, "datasets")?;
        let mut selected = child_table(datasets, dataset)?;
        if let Some(key) = snapshot.filter(|key| *key != "base") {
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(Error::message(format!("invalid snapshot key '{key}'")));
            }
            selected = child_table(child_table(selected, "snapshot")?, key)?;
        }
        let entry = child_table(child_table(selected, "pbf")?, variant)?;
        self.resolve_entry(host, role, variant, entry)
    }

    pub(crate) fn resolve_entry(
        &self,
        host: &str,
        role: &str,
        variant: &str,
        entry: &toml::Table,
    ) -> Result<ResolvedInput, Error> {
        let file = optional_string(entry, "file")?
            .filter(|file| !file.trim().is_empty())
            .ok_or_else(|| Error::message("file entry requires a non-empty 'file'"))?;
        if entry.contains_key("sha256") {
            return Err(Error::message(
                "dataset pin 'sha256' is retired; configure 'xxhash' instead",
            ));
        }
        let expected_digest = optional_string(entry, "xxhash")?
            .map(normalize_digest)
            .transpose()?;
        let path = self.data_dir(host)?.join(file);
        let observed_digest = file_digest(&path)?;
        if let Some(expected) = &expected_digest
            && expected != &observed_digest
        {
            return Err(Error::message(format!(
                "{}: xxhash mismatch: expected {expected}, observed {observed_digest}",
                path.display()
            )));
        }
        Ok(ResolvedInput {
            role: role.to_owned(),
            variant: variant.to_owned(),
            path,
            expected_digest,
            observed_digest,
        })
    }

    /// Host scratch path, using the same host namespace rules as dataset resolution.
    pub fn scratch_root(&self, host: &str) -> Result<PathBuf, Error> {
        let selected = self.selected_host(host)?;
        Ok(self
            .root
            .join(optional_string(selected, "scratch")?.unwrap_or("data/scratch")))
    }

    /// Host data directory: `data` (default "data") under the project root.
    pub fn data_dir(&self, host: &str) -> Result<PathBuf, Error> {
        let selected = self.selected_host(host)?;
        Ok(self
            .root
            .join(optional_string(selected, "data")?.unwrap_or("data")))
    }

    pub(crate) fn selected_host(&self, host: &str) -> Result<&toml::Table, Error> {
        let explicit = match self.table.get("host") {
            Some(value) => Some(
                value
                    .as_table()
                    .ok_or_else(|| Error::message("'host' must be a table"))?,
            ),
            None => None,
        }
        .and_then(|hosts| hosts.get(host));
        let legacy = self.table.get(host).filter(|_| !reserved_host(host));
        if explicit.is_some() && legacy.is_some() {
            return Err(Error::message(format!(
                "host '{host}' is configured both as [{host}] and [host.{host}]; keep one spelling"
            )));
        }
        explicit
            .or(legacy)
            .ok_or_else(|| Error::message(format!("unknown host '{host}'")))?
            .as_table()
            .ok_or_else(|| Error::message(format!("host '{host}' must be a table")))
    }
}

pub(crate) fn child_table<'a>(table: &'a toml::Table, key: &str) -> Result<&'a toml::Table, Error> {
    table
        .get(key)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| Error::message(format!("missing or invalid table '{key}'")))
}

pub(crate) fn optional_string<'a>(
    table: &'a toml::Table,
    key: &str,
) -> Result<Option<&'a str>, Error> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| Error::message(format!("'{key}' must be a string")))
        })
        .transpose()
}

pub(crate) fn normalize_digest(value: &str) -> Result<String, Error> {
    if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::message(format!(
            "invalid XXH3-128 digest '{value}': expected 32 hex digits"
        )));
    }
    Ok(value.to_ascii_lowercase())
}

// A reserved section can still be selected through [host.NAME].
pub(crate) fn reserved_host(name: &str) -> bool {
    matches!(
        name,
        "project"
            | "include"
            | "host"
            | "litehtml"
            | "sluggrs"
            | "ratatoskr"
            | "piners"
            | "dellingr"
            | "mogwai"
            | "dependency_rule"
            | "check"
            | "test"
            | "quarantine"
            | "capture_env"
            | "gremlins"
            | "header"
            | "textlint"
            | "textlint_preset"
            | "script_check"
            | "manifest"
            | "rustdoc"
            | "deps"
            | "clippy"
            | "lints"
            | "bin"
            | "member_check"
            | "fixture_lint"
            | "disable_toolchain"
            | "cargo_env"
            | "bench"
            | "bench_params"
            | "data"
    )
}

// Same stable-read check as brokkr's preflight digest path. This detects
// ordinary concurrent writes and replacements; it is not a file lock.
fn stamp(metadata: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}
