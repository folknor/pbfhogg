//! The per-dataset recovery journal: what this tool published under one
//! dataset key and may later adopt or replace.
//!
//! `DATA/.KEY.download.journal` holds one line per publication,
//! `NAME TAB KIND TAB ORIGIN TAB XXHASH`. A file at a final name is adopted
//! (raw, OSC) or replaced (indexed) only when a record matches its name,
//! kind, the request's origin and its current bytes, so a crash between
//! publishing and registering can never let another source, another kind
//! of output, or another dataset sharing the final name claim the file.
//!
//! The journal is a set. A pending publication is added, keeping every
//! earlier record of the name, before the output rename; after the rename
//! and directory fsync, the name's records of that kind with other digests
//! are retired. A crash anywhere between leaves both the old and the new
//! bytes recognisable. Each write replaces the whole journal atomically
//! (staged sibling, fsync, rename, directory fsync) under the dataset lock;
//! a lost, unreadable or legacy journal matches nothing, which refuses
//! recovery rather than misattributing it.

use std::path::{Path, PathBuf};

use pbfhogg_devkit::Config;

use crate::layout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Raw,
    Osc,
    Indexed,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Osc => "osc",
            Self::Indexed => "indexed",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "raw" => Some(Self::Raw),
            "osc" => Some(Self::Osc),
            "indexed" => Some(Self::Indexed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    name: String,
    kind: Kind,
    origin: String,
    digest: String,
}

pub(crate) struct Journal<'a> {
    config: &'a Config,
    host: &'a str,
    dir: &'a Path,
    key: &'a str,
    path: PathBuf,
    records: Vec<Record>,
}

impl<'a> Journal<'a> {
    /// Read the journal of dataset `key`; call only under its lock. A
    /// symlink or other non-regular file at the journal name refuses, so no
    /// other file can supply recovery evidence.
    pub(crate) fn load(
        config: &'a Config,
        host: &'a str,
        dir: &'a Path,
        key: &'a str,
    ) -> Result<Self, String> {
        let path = dir.join(format!(".{key}.download.journal"));
        let at = |e: std::io::Error| format!("{}: {e}", path.display());
        let records = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(at(e)),
            Ok(meta) if !meta.is_file() => {
                return Err(format!(
                    "{} is not a regular file; remove it and retry",
                    path.display()
                ));
            }
            Ok(_) => parse(&std::fs::read(&path).map_err(at)?),
        };
        Ok(Self {
            config,
            host,
            dir,
            key,
            path,
            records,
        })
    }

    /// Whether any record names `name` as `kind` from `origin`; when none
    /// does, the file need not be hashed to know it is not ours.
    pub(crate) fn mentions(&self, name: &str, kind: Kind, origin: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.name == name && r.kind == kind && r.origin == origin)
    }

    /// Whether a record vouches for `digest` at `name` as `kind` from `origin`.
    pub(crate) fn vouches(&self, name: &str, kind: Kind, origin: &str, digest: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.name == name && r.kind == kind && r.origin == origin && r.digest == digest)
    }

    /// Record a publication about to be renamed into `name`. Records of
    /// names with no file are pruned first, never the one added now: a first
    /// publication's destination is absent by design.
    pub(crate) fn pending(
        &mut self,
        name: &str,
        kind: Kind,
        origin: &str,
        digest: &str,
    ) -> Result<(), String> {
        let dir = self.dir;
        self.records
            .retain(|r| std::fs::symlink_metadata(dir.join(&r.name)).is_ok());
        let record = Record {
            name: name.to_owned(),
            kind,
            origin: origin.to_owned(),
            digest: digest.to_owned(),
        };
        if !self.records.contains(&record) {
            self.records.push(record);
        }
        self.write()
    }

    /// After `name` was published durably with `digest`, retire the name's
    /// other records of that kind.
    pub(crate) fn settle(&mut self, name: &str, kind: Kind, digest: &str) -> Result<(), String> {
        let before = self.records.len();
        self.records
            .retain(|r| !(r.name == name && r.kind == kind && r.digest != digest));
        if self.records.len() == before {
            return Ok(());
        }
        self.write()
    }

    fn write(&self) -> Result<(), String> {
        layout::claim(self.config, self.host, self.dir, &self.path)?;
        let staged = layout::stage(self.config, self.host, self.dir, self.key, &self.path, &[])?;
        let text: String = self
            .records
            .iter()
            .map(|r| {
                format!(
                    "{}\t{}\t{}\t{}\n",
                    r.name,
                    r.kind.as_str(),
                    r.origin,
                    r.digest
                )
            })
            .collect();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
            .map_err(|e| format!("{}: {e}", staged.display()))?;
        layout::publish(&staged, &self.path)
    }
}

/// Complete, well-formed lines only; anything else (a legacy single-origin
/// lock record, a torn line) is no record at all.
fn parse(bytes: &[u8]) -> Vec<Record> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    text.split_inclusive('\n')
        .filter_map(|line| {
            let line = line.strip_suffix('\n')?;
            let mut fields = line.split('\t');
            let record = Record {
                name: fields.next()?.to_owned(),
                kind: Kind::parse(fields.next()?)?,
                origin: fields.next()?.to_owned(),
                digest: fields.next()?.to_owned(),
            };
            let well_formed = fields.next().is_none()
                && !record.name.is_empty()
                && !record.name.contains('/')
                && !record.digest.is_empty();
            well_formed.then_some(record)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_complete_well_formed_lines_are_records() {
        let records = parse(
            b"download.geofabrik.de/asia/georgia\n\
              a.osm.pbf\traw\to\tff\n\
              b.osm.pbf\tbogus\to\tff\n\
              c.osm.pbf\traw\to\tff\textra\n\
              d.osm.pbf\tosc\to\tff",
        );
        assert_eq!(
            records,
            [Record {
                name: "a.osm.pbf".into(),
                kind: Kind::Raw,
                origin: "o".into(),
                digest: "ff".into(),
            }]
        );
    }

    #[test]
    fn pending_keeps_old_bytes_recognisable_until_settled() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(
            &path,
            format!("[h]\ndata = {:?}\n[h.datasets]\n", dir.to_str().unwrap()),
        )
        .unwrap();
        let config = Config::load(&path, None).unwrap();
        std::fs::write(dir.join("i.osm.pbf"), b"old").unwrap();
        let mut journal = Journal::load(&config, "h", &dir, "d").unwrap();
        journal
            .pending("i.osm.pbf", Kind::Indexed, "o", "old")
            .unwrap();
        journal
            .pending("gone.osm.pbf", Kind::Raw, "o", "x")
            .unwrap();
        journal
            .pending("i.osm.pbf", Kind::Indexed, "o", "new")
            .unwrap();
        // A crash here: the reloaded journal still knows both digests, and
        // the record of a name whose file never appeared was pruned.
        let mut journal = Journal::load(&config, "h", &dir, "d").unwrap();
        assert!(journal.vouches("i.osm.pbf", Kind::Indexed, "o", "old"));
        assert!(journal.vouches("i.osm.pbf", Kind::Indexed, "o", "new"));
        assert!(!journal.mentions("gone.osm.pbf", Kind::Raw, "o"));
        assert!(!journal.vouches("i.osm.pbf", Kind::Raw, "o", "new"));
        assert!(!journal.vouches("i.osm.pbf", Kind::Indexed, "p", "new"));
        journal.settle("i.osm.pbf", Kind::Indexed, "new").unwrap();
        let journal = Journal::load(&config, "h", &dir, "d").unwrap();
        assert!(!journal.vouches("i.osm.pbf", Kind::Indexed, "o", "old"));
        assert!(journal.vouches("i.osm.pbf", Kind::Indexed, "o", "new"));
    }
}
