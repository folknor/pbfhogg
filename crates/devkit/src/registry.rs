//! The host dataset registry: a typed read view of one dataset, and the one
//! writer that edits it.
//!
//! The writer is a port of brokkr's `DatasetToml`. Every edit is one
//! transaction: parse the file that owns the dataset's table into a
//! `toml_edit` document, apply the whole batch in memory, and commit once
//! with an atomic replace (sibling temp file, fsync, rename, directory
//! fsync). A failure anywhere before the commit leaves the file
//! byte-identical; comments and formatting of everything the edit does not
//! touch survive (`toml_edit` is lossless). Keys and values are written by
//! `toml_edit`, so a quote, backslash or dot in a name stays one key.
//!
//! The writer only emits keys brokkr's host schema accepts (`origin`,
//! `download_date`, `file`, `xxhash`, `seq`, `snapshot.KEY.*`), because brokkr
//! still reads these files and refuses unknown keys.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::datasets::normalize_digest;
use crate::{Config, Error};

/// One registered file: `file` relative to the host data directory (or
/// absolute), with its optional pin and replication sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub file: String,
    pub xxhash: Option<String>,
    pub seq: Option<u64>,
}

/// The primary data or one named snapshot of a dataset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generation {
    pub download_date: Option<String>,
    pub pbf: BTreeMap<String, FileEntry>,
    pub osc: BTreeMap<u64, FileEntry>,
}

/// What the registry says about one dataset of one host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatasetView {
    pub origin: Option<String>,
    pub primary: Generation,
    pub snapshots: BTreeMap<String, Generation>,
}

impl Config {
    /// All registered datasets for one host, for publication collision checks.
    pub fn dataset_views(&self, host: &str) -> Result<BTreeMap<String, DatasetView>, Error> {
        let table = self.selected_host(host)?;
        let Some(datasets) = table.get("datasets") else {
            return Ok(BTreeMap::new());
        };
        let datasets = datasets
            .as_table()
            .ok_or_else(|| Error::message("datasets must be a table"))?;
        datasets
            .keys()
            .map(|key| {
                self.dataset(host, key)?
                    .map(|view| (key.clone(), view))
                    .ok_or_else(|| Error::message("dataset disappeared"))
            })
            .collect()
    }

    /// The dataset `key` of `host`, or `None` when the host has no such
    /// dataset. An unknown host is an error.
    pub fn dataset(&self, host: &str, key: &str) -> Result<Option<DatasetView>, Error> {
        let host_table = self.selected_host(host)?;
        let Some(datasets) = host_table.get("datasets") else {
            return Ok(None);
        };
        let datasets = datasets
            .as_table()
            .ok_or_else(|| Error::message("'datasets' must be a table"))?;
        let Some(ds) = datasets.get(key) else {
            return Ok(None);
        };
        let ds = ds
            .as_table()
            .ok_or_else(|| Error::message(format!("dataset '{key}' must be a table")))?;
        let mut snapshots = BTreeMap::new();
        if let Some(snaps) = ds.get("snapshot") {
            let snaps = snaps
                .as_table()
                .ok_or_else(|| Error::message("'snapshot' must be a table"))?;
            for (name, snap) in snaps {
                let snap = snap
                    .as_table()
                    .ok_or_else(|| Error::message(format!("snapshot '{name}' must be a table")))?;
                snapshots.insert(name.clone(), generation(snap)?);
            }
        }
        Ok(Some(DatasetView {
            origin: string(ds, "origin")?,
            primary: generation(ds)?,
            snapshots,
        }))
    }

    /// The config file that owns `[HOST.datasets.KEY]` and whether the host
    /// uses the explicit `[host.HOST]` namespace. A dataset defined in exactly
    /// one file is edited there; one split across files is refused, because a
    /// rotation moves whole `pbf`/`osc` trees. A dataset not yet registered
    /// goes into the one file holding the host's datasets, else the project's
    /// own file.
    fn dataset_owner(&self, host: &str, key: &str) -> Result<(PathBuf, bool), Error> {
        let explicit = self
            .sources
            .iter()
            .any(|s| lookup(&s.table, &["host", host]).is_some());
        let legacy = !crate::datasets::reserved_host(host)
            && self.sources.iter().any(|s| s.table.contains_key(host));
        if explicit && legacy {
            return Err(Error::message(format!(
                "host '{host}' is configured both as [{host}] and [host.{host}]; keep one spelling"
            )));
        }
        let mut path: Vec<&str> = if explicit {
            vec!["host", host, "datasets", key]
        } else {
            vec![host, "datasets", key]
        };
        let holders: Vec<&Path> = self
            .sources
            .iter()
            .filter(|s| lookup(&s.table, &path).is_some())
            .map(|s| s.path.as_path())
            .collect();
        match holders.as_slice() {
            [one] => return Ok(((*one).to_path_buf(), explicit)),
            [] => {}
            many => {
                return Err(Error::message(format!(
                    "dataset '{key}' of host '{host}' is split across {}; keep it in one file",
                    many.iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        path.pop();
        let hosts: Vec<&Path> = self
            .sources
            .iter()
            .filter(|s| lookup(&s.table, &path).is_some())
            .map(|s| s.path.as_path())
            .collect();
        let owner = match hosts.as_slice() {
            [one] => (*one).to_path_buf(),
            _ => self
                .sources
                .first()
                .map(|s| s.path.clone())
                .ok_or_else(|| Error::message("config has no project file"))?,
        };
        Ok((owner, explicit))
    }

    /// Open one registry transaction on the file owning `HOST.datasets.KEY`.
    pub fn edit_dataset(&self, host: &str, key: &str) -> Result<DatasetEdit, Error> {
        let (path, explicit) = self.dataset_owner(host, key)?;
        let at = |e: &dyn std::fmt::Display| Error::message(format!("{}: {e}", path.display()));
        let text = std::fs::read_to_string(&path).map_err(|e| at(&e))?;
        let doc: toml_edit::DocumentMut = text.parse().map_err(|e| at(&e))?;
        let mut base = Vec::new();
        if explicit {
            base.push("host".to_owned());
        }
        base.extend([host.to_owned(), "datasets".to_owned(), key.to_owned()]);
        Ok(DatasetEdit {
            path,
            doc,
            base,
            changes: Vec::new(),
        })
    }
}

fn lookup<'a>(table: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Value> {
    let (first, rest) = path.split_first()?;
    let mut value = table.get(*first)?;
    for key in rest {
        value = value.as_table()?.get(*key)?;
    }
    Some(value)
}

fn string(table: &toml::Table, key: &str) -> Result<Option<String>, Error> {
    table
        .get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::message(format!("'{key}' must be a string")))
        })
        .transpose()
}

fn generation(table: &toml::Table) -> Result<Generation, Error> {
    let mut pbf = BTreeMap::new();
    if let Some(variants) = table.get("pbf") {
        let variants = variants
            .as_table()
            .ok_or_else(|| Error::message("'pbf' must be a table"))?;
        for (variant, entry) in variants {
            pbf.insert(variant.clone(), file_entry(entry)?);
        }
    }
    let mut osc = BTreeMap::new();
    if let Some(seqs) = table.get("osc") {
        let seqs = seqs
            .as_table()
            .ok_or_else(|| Error::message("'osc' must be a table"))?;
        for (seq, entry) in seqs {
            let seq: u64 = seq
                .parse()
                .map_err(|_| Error::message(format!("osc key '{seq}' is not a sequence")))?;
            osc.insert(seq, file_entry(entry)?);
        }
    }
    Ok(Generation {
        download_date: string(table, "download_date")?,
        pbf,
        osc,
    })
}

fn file_entry(value: &toml::Value) -> Result<FileEntry, Error> {
    let table = value
        .as_table()
        .ok_or_else(|| Error::message("a file entry must be a table"))?;
    let file = string(table, "file")?
        .filter(|f| !f.trim().is_empty())
        .ok_or_else(|| Error::message("a file entry requires a non-empty 'file'"))?;
    let xxhash = string(table, "xxhash")?
        .map(|d| normalize_digest(&d))
        .transpose()?;
    let seq = table
        .get("seq")
        .map(|v| {
            v.as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| Error::message("'seq' must be a non-negative integer"))
        })
        .transpose()?;
    Ok(FileEntry { file, xxhash, seq })
}

/// One read-modify-write transaction on the file owning a dataset table.
/// Dropping it without [`DatasetEdit::commit`] writes nothing.
#[derive(Debug)]
pub struct DatasetEdit {
    path: PathBuf,
    doc: toml_edit::DocumentMut,
    /// `[host.]HOST.datasets.KEY` as table keys.
    base: Vec<String>,
    /// One narration line per change, returned only once the commit lands.
    changes: Vec<String>,
}

impl DatasetEdit {
    /// The file this transaction edits.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn label(&self, rest: &[&str]) -> String {
        let mut parts = self.base.clone();
        parts.extend(rest.iter().map(|p| (*p).to_owned()));
        format!("[{}]", parts.join("."))
    }

    /// The table at `base.rest...`, creating missing levels as implicit
    /// tables (no header of their own).
    fn table_mut(&mut self, rest: &[&str]) -> Result<&mut toml_edit::Table, Error> {
        let mut path = self.base.clone();
        path.extend(rest.iter().map(|p| (*p).to_owned()));
        let shown = self.path.display().to_string();
        let mut table = self.doc.as_table_mut();
        for (depth, key) in path.iter().enumerate() {
            if !table.contains_key(key) {
                let mut fresh = toml_edit::Table::new();
                fresh.set_implicit(true);
                table.insert(key, toml_edit::Item::Table(fresh));
            }
            table = table
                .get_mut(key)
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| {
                    Error::message(format!(
                        "{shown}: [{}] is not a table (inline tables and dotted keys are \
                         not supported here)",
                        path[..=depth].join(".")
                    ))
                })?;
        }
        Ok(table)
    }

    /// Make the dataset table explicit and set its `origin`.
    pub fn set_origin(&mut self, origin: &str) -> Result<(), Error> {
        let label = self.label(&[]);
        let ds = self.table_mut(&[])?;
        ds.set_implicit(false);
        ds.insert("origin", toml_edit::value(origin));
        self.changes.push(format!("set origin of {label}"));
        Ok(())
    }

    /// Set the primary data's `download_date` (`YYYY-MM-DD`).
    pub fn set_download_date(&mut self, date: &str) -> Result<(), Error> {
        let label = self.label(&[]);
        let ds = self.table_mut(&[])?;
        ds.set_implicit(false);
        ds.insert("download_date", toml_edit::value(date));
        self.changes.push(format!("set download_date of {label}"));
        Ok(())
    }

    fn set_file_entry(&mut self, rest: &[&str], key: &str, entry: &FileEntry) -> Result<(), Error> {
        let mut full = rest.to_vec();
        full.push(key);
        let label = self.label(&full);
        let parent = self.table_mut(rest)?;
        let mut table = toml_edit::Table::new();
        table.decor_mut().set_prefix("\n");
        table.insert("file", toml_edit::value(entry.file.as_str()));
        if let Some(xxhash) = &entry.xxhash {
            table.insert("xxhash", toml_edit::value(xxhash.as_str()));
        }
        if let Some(seq) = entry.seq {
            let seq = i64::try_from(seq)
                .map_err(|_| Error::message(format!("seq {seq} does not fit a TOML integer")))?;
            table.insert("seq", toml_edit::value(seq));
        }
        parent.insert(key, toml_edit::Item::Table(table));
        self.changes.push(format!("added {label}"));
        Ok(())
    }

    /// `[..pbf.VARIANT]` on the primary data.
    pub fn set_pbf(&mut self, variant: &str, entry: &FileEntry) -> Result<(), Error> {
        self.set_file_entry(&["pbf"], variant, entry)
    }

    /// Insert a sequence on an existing primary PBF entry, never replacing one.
    pub fn set_pbf_seq(&mut self, variant: &str, seq: u64) -> Result<(), Error> {
        self.set_seq(&["pbf"], variant, seq)
    }

    /// Insert a sequence on an existing snapshot PBF, never replacing one.
    pub fn set_snapshot_pbf_seq(
        &mut self,
        snap: &str,
        variant: &str,
        seq: u64,
    ) -> Result<(), Error> {
        self.set_seq(&["snapshot", snap, "pbf"], variant, seq)
    }

    fn set_seq(&mut self, rest: &[&str], variant: &str, seq: u64) -> Result<(), Error> {
        let label = self.label(&[rest, &[variant]].concat());
        let seq = i64::try_from(seq)
            .map_err(|_| Error::message("sequence does not fit a TOML integer"))?;
        let parent = self.table_mut(rest)?;
        let table = parent
            .get_mut(variant)
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| Error::message(format!("{label} does not exist")))?;
        if table.contains_key("seq") {
            return Err(Error::message(format!("{label} already has seq")));
        }
        table.insert("seq", toml_edit::value(seq));
        self.changes.push(format!("set seq of {label}"));
        Ok(())
    }

    /// `[..osc.SEQ]` on the primary data.
    pub fn set_osc(&mut self, seq: u64, entry: &FileEntry) -> Result<(), Error> {
        self.set_file_entry(&["osc"], &seq.to_string(), entry)
    }

    /// `[..snapshot.KEY]` with its `download_date`.
    pub fn set_snapshot_header(&mut self, snap: &str, date: &str) -> Result<(), Error> {
        let label = self.label(&["snapshot", snap]);
        let table = self.table_mut(&["snapshot", snap])?;
        table.set_implicit(false);
        table.insert("download_date", toml_edit::value(date));
        if table.decor().prefix().is_none() {
            table.decor_mut().set_prefix("\n");
        }
        self.changes.push(format!("added {label}"));
        Ok(())
    }

    /// Remove an entire snapshot, including OSCs and extension tables.
    /// The caller must explicitly authorize replacement before calling this.
    pub fn remove_snapshot(&mut self, snap: &str) -> Result<(), Error> {
        let label = self.label(&["snapshot", snap]);
        self.table_mut(&["snapshot"])?.remove(snap);
        self.changes.push(format!("removed {label}"));
        Ok(())
    }

    /// `[..snapshot.KEY.pbf.VARIANT]`.
    pub fn set_snapshot_pbf(
        &mut self,
        snap: &str,
        variant: &str,
        entry: &FileEntry,
    ) -> Result<(), Error> {
        self.set_file_entry(&["snapshot", snap, "pbf"], variant, entry)
    }

    /// `[..snapshot.KEY.osc.SEQ]`.
    pub fn set_snapshot_osc(
        &mut self,
        snap: &str,
        seq: u64,
        entry: &FileEntry,
    ) -> Result<(), Error> {
        self.set_file_entry(&["snapshot", snap, "osc"], &seq.to_string(), entry)
    }

    /// Move the primary `pbf`/`osc` tables under `snapshot.SNAP` (bodies,
    /// `seq` and comments kept), give that snapshot `old_date`, and set the
    /// dataset's own `download_date` to `new_date`. The snapshot header is
    /// pinned just ahead of the first moved table so it renders above them.
    pub fn rotate_primary_to_snapshot(
        &mut self,
        snap: &str,
        old_date: &str,
        new_date: &str,
    ) -> Result<(), Error> {
        let label = self.label(&[]);
        let ds = self.table_mut(&[])?;
        let pbf = ds.remove("pbf");
        let osc = ds.remove("osc");
        ds.set_implicit(false);
        ds.insert("download_date", toml_edit::value(new_date));
        let first = [pbf.as_ref(), osc.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(min_table_position)
            .min();
        self.set_snapshot_header(snap, old_date)?;
        let table = self.table_mut(&["snapshot", snap])?;
        if let Some(pos) = first {
            table.set_position(Some(pos));
        }
        if let Some(item) = pbf {
            table.insert("pbf", item);
        }
        if let Some(item) = osc {
            table.insert("osc", item);
        }
        self.changes
            .push(format!("rotated {label} pbf/osc tables -> snapshot.{snap}"));
        Ok(())
    }

    /// Atomically replace the file with the edited document. Returns one
    /// line per change, for the caller to narrate.
    pub fn commit(self) -> Result<Vec<String>, Error> {
        if self.changes.is_empty() {
            return Ok(Vec::new());
        }
        replace_file(&self.path, self.doc.to_string().as_bytes())?;
        let shown = self.path.display().to_string();
        Ok(self
            .changes
            .into_iter()
            .map(|c| format!("{c} in {shown}"))
            .collect())
    }
}

fn min_table_position(item: &toml_edit::Item) -> Option<isize> {
    let table = item.as_table()?;
    let children = table
        .iter()
        .filter_map(|(_, child)| min_table_position(child))
        .min();
    match (table.position(), children) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Replace `path` with `bytes` durably: write a sibling temp file, fsync it,
/// rename it over `path`, fsync the directory. A failure leaves `path` as it
/// was; a stale temp from a killed run is overwritten by the next one.
pub fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let at = |e: std::io::Error| Error::message(format!("{}: {e}", path.display()));
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::message(format!("{}: no file name", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        std::fs::remove_file(&tmp).ok();
    }
    result.map_err(at)
}
