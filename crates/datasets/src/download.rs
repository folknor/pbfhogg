//! `download REGION`: acquire a dataset's primary PBF, generate its indexed
//! variant unless `--no-indexed`, and register what was produced.
//!
//! Registration follows the desired state, not this run's work log: every
//! file the dataset should have and does not register is produced or, when
//! a complete file already sits at its target name from a run that died
//! before registering, adopted. A raw PBF or OSC is adopted only when the
//! recovery journal (`journal`) vouches for its name, kind, source and
//! bytes; an unregistered indexed file is never adopted, always regenerated
//! from the raw being registered, and replaced only when the journal
//! vouches for it the same way. No final name a registration on the host
//! resolves through is ever written or adopted (`layout::claim`). A
//! registered entry whose file is missing, or fails its pin, refuses: the
//! tool never rewrites a pin.

use std::path::{Path, PathBuf};

use pbfhogg_devkit::{Config, FileEntry, Generation, file_digest};

use crate::date::CivilDate;
use crate::journal::{Journal, Kind};
use crate::layout::{
    claim, indexed_pbf_filename, make_durable, osc_filename, publish, raw_pbf_filename, stage,
};
use crate::lock::DatasetLock;
use crate::output::note;
use crate::process::Programs;
use crate::source::{Source, resolve};

pub(crate) struct Request<'a> {
    pub(crate) config: &'a Config,
    pub(crate) host: &'a str,
    pub(crate) region: &'a str,
    pub(crate) osc_seq: Option<u64>,
    pub(crate) refresh: bool,
    pub(crate) force: bool,
    pub(crate) as_snapshot: Option<&'a str>,
    pub(crate) snapshot: Option<&'a str>,
    /// Produce and register the indexed variant of a newly produced raw.
    /// False (`--no-indexed`) keeps and verifies indexed entries already
    /// registered, but never generates one.
    pub(crate) indexed: bool,
    pub(crate) tools: &'a dyn Programs,
    pub(crate) today: CivilDate,
}

/// A registered file, checked: present and matching its pin.
fn registered(data_dir: &Path, entry: &FileEntry, what: &str) -> Result<PathBuf, String> {
    let path = data_dir.join(&entry.file);
    if !nonempty(&path) {
        return Err(format!(
            "{what} is registered as {} but the file is missing or empty",
            path.display()
        ));
    }
    if let Some(pin) = &entry.xxhash {
        let observed = file_digest(&path).map_err(|e| e.to_string())?;
        if &observed != pin {
            return Err(format!(
                "{}: xxhash mismatch: registered {pin}, observed {observed}",
                path.display()
            ));
        }
    }
    Ok(path)
}

fn nonempty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
}

/// The indexed name that pairs with a raw file name.
fn indexed_name_for(raw: &str, key: &str, today: CivilDate) -> String {
    raw.strip_suffix(".osm.pbf").map_or_else(
        || indexed_pbf_filename(key, &today.yyyymmdd()),
        |stem| format!("{stem}-with-indexdata.osm.pbf"),
    )
}

/// A file this run produced or adopted, to be registered: its name, path
/// and observed digest.
type Produced = (String, PathBuf, String);
type New = Option<Produced>;

/// Refuse before any transfer when a registration on the host resolves
/// through one of a run's known final names.
fn preflight_names(req: &Request<'_>, dir: &Path, names: &[String]) -> Result<(), String> {
    for name in names {
        claim(req.config, req.host, dir, &dir.join(name))?;
    }
    Ok(())
}

/// Produce `name` of `kind`, or adopt a complete file this tool published
/// there before. `produce` fills the staged path. An existing file is
/// adopted (raw, OSC) or replaced (indexed) only when the journal vouches
/// for its name, kind, origin and bytes; anything else at the name refuses.
#[expect(
    clippy::too_many_arguments,
    reason = "one call shape for every producer"
)]
fn obtain(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    dir: &Path,
    journal: &mut Journal<'_>,
    name: &str,
    kind: Kind,
    protect: &[&Path],
    produce: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<Produced, String> {
    let key = resolved.key.as_str();
    let origin = resolved.source.origin();
    let dest = dir.join(name);
    claim(req.config, req.host, dir, &dest)?;
    match std::fs::symlink_metadata(&dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", dest.display())),
        Ok(meta) => {
            let digest =
                if meta.is_file() && meta.len() > 0 && journal.mentions(name, kind, &origin) {
                    Some(file_digest(&dest).map_err(|e| e.to_string())?)
                } else {
                    None
                };
            let Some(digest) = digest.filter(|d| journal.vouches(name, kind, &origin, d)) else {
                return Err(format!(
                    "{} is not registered and was not published as {} from {origin} by this \
                     tool for '{key}'; remove it and retry",
                    dest.display(),
                    match kind {
                        Kind::Raw => "a raw PBF",
                        Kind::Osc => "an OSC",
                        Kind::Indexed => "an indexed PBF",
                    }
                ));
            };
            if kind != Kind::Indexed {
                note(&format!(
                    "  ADOPT (complete, unregistered): {}",
                    dest.display()
                ));
                make_durable(&dest)?;
                return Ok((name.to_owned(), dest, digest));
            }
        }
    }
    let staged = stage(req.config, req.host, dir, key, &dest, protect)?;
    produce(&staged)?;
    let digest = file_digest(&staged).map_err(|e| e.to_string())?;
    journal.pending(name, kind, &origin, &digest)?;
    publish(&staged, &dest)?;
    journal.settle(name, kind, &digest)?;
    Ok((name.to_owned(), dest, digest))
}

/// Fetch (or adopt) the raw PBF named for `suffix`.
fn acquire_raw(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    data_dir: &Path,
    lock: &DatasetLock,
    journal: &mut Journal<'_>,
    suffix: &str,
) -> Result<Produced, String> {
    let name = raw_pbf_filename(&resolved.key, suffix);
    let url = resolved.source.pbf_url();
    obtain(
        req,
        resolved,
        data_dir,
        journal,
        &name,
        Kind::Raw,
        &[],
        |staged| {
            note(&format!(
                "  GET: {url} -> {}",
                data_dir.join(&name).display()
            ));
            req.tools.fetch(&url, staged, lock)
        },
    )
}

/// Generate the indexed variant of `raw` with the handed pbfhogg binary.
fn generate_indexed(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    raw: &Path,
    data_dir: &Path,
    lock: &DatasetLock,
    journal: &mut Journal<'_>,
) -> Result<Produced, String> {
    let raw_name = raw.file_name().unwrap_or_default().to_string_lossy();
    let name = indexed_name_for(&raw_name, &resolved.key, req.today);
    obtain(
        req,
        resolved,
        data_dir,
        journal,
        &name,
        Kind::Indexed,
        &[raw],
        |staged| {
            note(&format!(
                "  generating indexed PBF via pbfhogg cat -> {}",
                data_dir.join(&name).display()
            ));
            req.tools.cat(raw, staged, lock)
        },
    )
}

#[derive(Default)]
struct Osc {
    learned: Option<u64>,
    files: Vec<(u64, Produced)>,
}

/// Configured raw seq wins. Headers only establish continuity for the exact
/// regional stream; planet's PBF header uses minutes, while we fetch days.
fn base_seq(
    source: &Source,
    configured: Option<u64>,
    header: Option<(&str, &str)>,
) -> Result<(u64, bool), String> {
    if let Some(seq) = configured {
        return Ok((seq, false));
    }
    if !matches!(source, Source::Planet)
        && let Some((seq, url)) = header
        && url.trim_end_matches('/') == source.stream_url()
        && let Ok(seq) = seq.parse::<u64>()
    {
        return Ok((seq, true));
    }
    Err("OSC acquisition needs seq on pbf.raw; no matching replication stream sequence in its header".into())
}

fn missing_seqs(generation: &Generation, base: u64, target: u64) -> impl Iterator<Item = u64> + '_ {
    (base..target)
        .map(|n| n + 1)
        .filter(|seq| !generation.osc.contains_key(seq))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the OSC producer's whole context"
)]
fn acquire_osc(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    raw: &Path,
    dir: &Path,
    lock: &DatasetLock,
    journal: &mut Journal<'_>,
    target: u64,
    generation: &Generation,
) -> Result<Osc, String> {
    let configured = generation.pbf.get("raw").and_then(|e| e.seq);
    let header = if configured.is_none() && !matches!(resolved.source, Source::Planet) {
        Some((
            req.tools
                .inspect_get(raw, "header.replication.sequence", lock)
                .map_err(|e| format!("OSC acquisition needs seq on pbf.raw: {e}"))?,
            req.tools
                .inspect_get(raw, "header.replication.url", lock)
                .map_err(|e| format!("OSC acquisition needs seq on pbf.raw: {e}"))?,
        ))
    } else {
        None
    };
    let (base, learned) = base_seq(
        &resolved.source,
        configured,
        header.as_ref().map(|(s, u)| (s.as_str(), u.as_str())),
    )?;
    // Every configured diff in the requested chain must still satisfy its pin.
    for (seq, entry) in generation.osc.range(..=target) {
        if *seq > base {
            registered(dir, entry, &format!("osc.{seq}"))?;
        }
    }
    let mut osc = Osc {
        learned: learned.then_some(base),
        files: Vec::new(),
    };
    for seq in missing_seqs(generation, base, target) {
        if let Some(view) = &resolved.dataset {
            let mut shared: Option<(String, PathBuf, String)> = None;
            for g in std::iter::once(&view.primary).chain(view.snapshots.values()) {
                if let Some(entry) = g.osc.get(&seq) {
                    let path = registered(dir, entry, &format!("shared osc.{seq}"))?;
                    let digest = file_digest(&path).map_err(|e| e.to_string())?;
                    if shared.as_ref().is_some_and(|(_, _, old)| old != &digest) {
                        return Err(format!("conflicting registered copies of osc.{seq}"));
                    }
                    shared = Some((entry.file.clone(), path, digest));
                }
            }
            // A registered donor is referenced, never written, so the
            // final-name rule does not apply to it.
            if let Some(shared) = shared {
                osc.files.push((seq, shared));
                continue;
            }
        }
        let name = osc_filename(&resolved.key, seq);
        let url = resolved.source.osc_url(seq);
        let produced = obtain(
            req,
            resolved,
            dir,
            journal,
            &name,
            Kind::Osc,
            &[raw],
            |staged| {
                note(&format!("  GET: {url} -> {}", dir.join(&name).display()));
                req.tools.fetch(&url, staged, lock)
            },
        )?;
        osc.files.push((seq, produced));
    }
    Ok(osc)
}

/// Refuse before any transfer when `--osc-seq` will have to learn the raw's
/// sequence from its header and no CLI was handed to read it.
fn preflight_seq_learning(
    req: &Request<'_>,
    source: &Source,
    configured: Option<u64>,
) -> Result<(), String> {
    if req.osc_seq.is_some()
        && configured.is_none()
        && !matches!(source, Source::Planet)
        && !req.tools.has_cli()
    {
        return Err(format!(
            "--osc-seq must learn the raw header sequence: {}",
            crate::process::NO_CLI
        ));
    }
    Ok(())
}

fn refresh_archive(primary: &Generation, raw: &Path) -> Result<(CivilDate, i64), String> {
    let old_date = primary.download_date.as_deref().and_then(|date| {
        if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
            CivilDate::parse_yyyymmdd(&date.replace('-', ""))
        } else {
            None
        }
    });
    let local = match old_date {
        Some(date) => date
            .to_unix_seconds(0, 0, 0)
            .ok_or("invalid archive date")?,
        None => {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(raw).map_err(|e| e.to_string())?.mtime()
        }
    };
    let archive_date = old_date
        .or_else(|| CivilDate::from_unix_timestamp(local))
        .ok_or("invalid raw mtime")?;
    Ok((archive_date, local))
}

fn run_refresh(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    dir: &Path,
    lock: &DatasetLock,
) -> Result<(), String> {
    let view = resolved
        .dataset
        .as_ref()
        .ok_or("refresh requires an existing dataset")?;
    let primary = &view.primary;
    let raw_entry = primary.pbf.get("raw").ok_or("refresh requires pbf.raw")?;
    // Rotation retains every entry, so check every retained file and pin.
    for entry in primary.pbf.values().chain(primary.osc.values()) {
        registered(dir, entry, "primary")?;
    }
    let raw = dir.join(&raw_entry.file);
    let (archive_date, local) = refresh_archive(primary, &raw)?;
    let snap = archive_date.yyyymmdd();
    if view.snapshots.contains_key(&snap) {
        return Err(format!("snapshot '{snap}' already exists"));
    }
    let upstream = req.tools.head(&resolved.source.pbf_url(), lock)?;
    if !req.force && upstream.is_none_or(|time| time <= local) {
        note("  no evidence of a newer upstream file; use --force to rotate anyway");
        return Ok(());
    }
    if archive_date == req.today {
        return Err("same-day refresh is unsupported".into());
    }
    let suffix = req.today.yyyymmdd();
    preflight_seq_learning(req, &resolved.source, None)?;
    let mut destinations = vec![raw_pbf_filename(&resolved.key, &suffix)];
    if req.indexed {
        destinations.push(indexed_pbf_filename(&resolved.key, &suffix));
    }
    preflight_names(req, dir, &destinations)?;
    let mut journal = Journal::load(req.config, req.host, dir, &resolved.key)?;
    let raw_out = acquire_raw(req, resolved, dir, lock, &mut journal, &suffix)?;
    let raw = raw_out.1.clone();
    let indexed = if req.indexed {
        Some(generate_indexed(
            req,
            resolved,
            &raw,
            dir,
            lock,
            &mut journal,
        )?)
    } else {
        None
    };
    let osc = if let Some(target) = req.osc_seq {
        acquire_osc(
            req,
            resolved,
            &raw,
            dir,
            lock,
            &mut journal,
            target,
            &Generation::default(),
        )?
    } else {
        Osc::default()
    };
    let entries: Vec<_> = std::iter::once(("raw", raw_out))
        .chain(indexed.map(|produced| ("indexed", produced)))
        .map(|(variant, (file, _, digest))| {
            (
                variant,
                FileEntry {
                    file,
                    xxhash: Some(digest),
                    seq: osc.learned,
                },
            )
        })
        .collect();
    let mut edit = req
        .config
        .edit_dataset(req.host, &resolved.key)
        .map_err(|e| e.to_string())?;
    edit.rotate_primary_to_snapshot(&snap, &archive_date.iso(), &req.today.iso())
        .map_err(|e| e.to_string())?;
    for (variant, entry) in entries {
        edit.set_pbf(variant, &entry).map_err(|e| e.to_string())?;
    }
    for (seq, (file, _, digest)) in osc.files {
        edit.set_osc(
            seq,
            &FileEntry {
                file,
                xxhash: Some(digest),
                seq: None,
            },
        )
        .map_err(|e| e.to_string())?;
    }
    for change in edit.commit().map_err(|e| e.to_string())? {
        note(&format!("  {change}"));
    }
    Ok(())
}

pub(crate) fn run(req: &Request<'_>) -> Result<(), String> {
    let resolved = resolve(req.region, |key| {
        req.config.dataset(req.host, key).map_err(|e| e.to_string())
    })?;
    if let Some(snap) = req.as_snapshot {
        validate_snapshot(&resolved, snap)?;
    }
    let key = resolved.key.as_str();
    let data_dir = req.config.data_dir(req.host).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let lock = DatasetLock::acquire(req.config, req.host, &data_dir, key, &[])?;
    note(&format!("=== {key} ({}) ===", resolved.source.origin()));

    if let Some(snap) = req.snapshot {
        return extend_snapshot(req, &resolved, &data_dir, &lock, snap);
    }
    if req.refresh {
        return run_refresh(req, &resolved, &data_dir, &lock);
    }
    if let Some(snap) = req.as_snapshot {
        return run_snapshot(req, &resolved, &data_dir, &lock, snap);
    }
    let primary = resolved.dataset.as_ref().map(|v| &v.primary);
    let raw_entry = primary.and_then(|g| g.pbf.get("raw"));
    let indexed_entry = primary.and_then(|g| g.pbf.get("indexed"));
    if let Some(entry) = indexed_entry {
        registered(&data_dir, entry, "pbf.indexed")?;
        if !req.indexed && raw_entry.is_none() {
            return Err(format!(
                "'{key}' registers pbf.indexed but no pbf.raw; --no-indexed has no raw to keep"
            ));
        }
    }
    preflight_seq_learning(req, &resolved.source, raw_entry.and_then(|e| e.seq))?;
    let mut journal = Journal::load(req.config, req.host, &data_dir, key)?;
    let mut new_raw: New = None;
    let raw_path = if let Some(entry) = raw_entry {
        note(&format!("  SKIP (registered): {}", entry.file));
        Some(registered(&data_dir, entry, "pbf.raw")?)
    } else if indexed_entry.is_some() {
        // An indexed-only dataset has nothing to derive and nothing to fetch.
        None
    } else {
        let suffix = req.today.yyyymmdd();
        let mut names = vec![raw_pbf_filename(key, &suffix)];
        if req.indexed {
            names.push(indexed_pbf_filename(key, &suffix));
        }
        preflight_names(req, &data_dir, &names)?;
        let produced = acquire_raw(req, &resolved, &data_dir, &lock, &mut journal, &suffix)?;
        let dest = produced.1.clone();
        new_raw = Some(produced);
        Some(dest)
    };
    let new_indexed: New = match (indexed_entry, &raw_path) {
        (None, Some(raw)) if req.indexed => Some(generate_indexed(
            req,
            &resolved,
            raw,
            &data_dir,
            &lock,
            &mut journal,
        )?),
        _ => None,
    };
    let osc = if let Some(target) = req.osc_seq {
        let raw = raw_path
            .as_deref()
            .ok_or("OSC acquisition requires pbf.raw with a base seq")?;
        acquire_osc(
            req,
            &resolved,
            raw,
            &data_dir,
            &lock,
            &mut journal,
            target,
            primary.unwrap_or(&Generation::default()),
        )?
    } else {
        Osc::default()
    };
    register(req, &resolved, primary, &new_raw, &new_indexed, &osc)
}

/// Extend only the selected generation; never fetch or derive a PBF here.
fn extend_snapshot(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    dir: &Path,
    lock: &DatasetLock,
    snap: &str,
) -> Result<(), String> {
    let generation = resolved
        .dataset
        .as_ref()
        .and_then(|v| v.snapshots.get(snap))
        .ok_or_else(|| format!("snapshot '{snap}' is not registered"))?;
    let target = req.osc_seq.ok_or("--snapshot requires --osc-seq")?;
    let raw_entry = generation
        .pbf
        .get("raw")
        .ok_or("OSC acquisition requires snapshot pbf.raw")?;
    let raw = registered(dir, raw_entry, "snapshot pbf.raw")?;
    for entry in generation.pbf.values() {
        registered(dir, entry, "snapshot pbf")?;
    }
    let mut journal = Journal::load(req.config, req.host, dir, &resolved.key)?;
    let osc = acquire_osc(
        req,
        resolved,
        &raw,
        dir,
        lock,
        &mut journal,
        target,
        generation,
    )?;
    let mut edit = req
        .config
        .edit_dataset(req.host, &resolved.key)
        .map_err(|e| e.to_string())?;
    if let Some(seq) = osc.learned {
        for (variant, entry) in &generation.pbf {
            if entry.seq.is_none() && matches!(variant.as_str(), "raw" | "indexed") {
                edit.set_snapshot_pbf_seq(snap, variant, seq)
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    for (seq, (file, _, digest)) in osc.files {
        edit.set_snapshot_osc(
            snap,
            seq,
            &FileEntry {
                file,
                xxhash: Some(digest),
                seq: None,
            },
        )
        .map_err(|e| e.to_string())?;
    }
    for change in edit.commit().map_err(|e| e.to_string())? {
        note(&format!("  {change}"));
    }
    Ok(())
}

fn validate_snapshot(resolved: &crate::source::Resolved, snap: &str) -> Result<(), String> {
    if snap.is_empty()
        || snap == "base"
        || !snap
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("snapshot key must match [A-Za-z0-9_-]+ and must not be 'base'".into());
    }
    let dataset = resolved.dataset.as_ref().ok_or_else(|| {
        format!(
            "dataset '{}' not found; download its primary first",
            resolved.key
        )
    })?;
    if dataset.snapshots.contains_key(snap) {
        return Err(format!("snapshot '{snap}' is already registered"));
    }
    Ok(())
}

fn run_snapshot(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    dir: &Path,
    lock: &DatasetLock,
    snap: &str,
) -> Result<(), String> {
    let generation = Generation::default();
    preflight_seq_learning(req, &resolved.source, None)?;
    // Check every known output name before any producer runs.
    let mut outputs = vec![raw_pbf_filename(&resolved.key, snap)];
    if req.indexed {
        outputs.push(indexed_pbf_filename(&resolved.key, snap));
    }
    preflight_names(req, dir, &outputs)?;
    let mut journal = Journal::load(req.config, req.host, dir, &resolved.key)?;
    let raw_out = acquire_raw(req, resolved, dir, lock, &mut journal, snap)?;
    let raw = raw_out.1.clone();
    let indexed = if req.indexed {
        Some(generate_indexed(
            req,
            resolved,
            &raw,
            dir,
            lock,
            &mut journal,
        )?)
    } else {
        None
    };
    let osc = match req.osc_seq {
        Some(target) => acquire_osc(
            req,
            resolved,
            &raw,
            dir,
            lock,
            &mut journal,
            target,
            &generation,
        )?,
        None => Osc::default(),
    };
    let mut edit = req
        .config
        .edit_dataset(req.host, &resolved.key)
        .map_err(|e| e.to_string())?;
    let date = CivilDate::parse_yyyymmdd(snap).unwrap_or(req.today);
    edit.set_snapshot_header(snap, &date.iso())
        .map_err(|e| e.to_string())?;
    let produced =
        std::iter::once(("raw", raw_out)).chain(indexed.map(|produced| ("indexed", produced)));
    for (variant, (name, _, digest)) in produced {
        let entry = FileEntry {
            file: name,
            xxhash: Some(digest),
            seq: osc.learned,
        };
        edit.set_snapshot_pbf(snap, variant, &entry)
            .map_err(|e| e.to_string())?;
    }
    for (seq, (name, _, digest)) in osc.files {
        let entry = FileEntry {
            file: name,
            xxhash: Some(digest),
            seq: None,
        };
        edit.set_snapshot_osc(snap, seq, &entry)
            .map_err(|e| e.to_string())?;
    }
    for change in edit.commit().map_err(|e| e.to_string())? {
        note(&format!("  {change}"));
    }
    Ok(())
}

fn register(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    primary: Option<&pbfhogg_devkit::Generation>,
    new_raw: &New,
    new_indexed: &New,
    osc: &Osc,
) -> Result<(), String> {
    let key = resolved.key.as_str();
    let view = resolved.dataset.as_ref();
    let mut edit = req
        .config
        .edit_dataset(req.host, key)
        .map_err(|e| e.to_string())?;
    if view.is_none() {
        edit.set_origin(&resolved.source.origin())
            .map_err(|e| e.to_string())?;
    }
    for (variant, new) in [("raw", new_raw), ("indexed", new_indexed)] {
        if let Some((name, _, digest)) = new {
            let entry = FileEntry {
                file: name.clone(),
                xxhash: Some(digest.clone()),
                seq: None,
            };
            edit.set_pbf(variant, &entry).map_err(|e| e.to_string())?;
        }
    }
    // A learned seq goes on each variant the resulting generation holds,
    // registered before or produced now, that lacks one.
    if let Some(seq) = osc.learned {
        for (variant, new) in [("raw", new_raw), ("indexed", new_indexed)] {
            let existing = primary.and_then(|g| g.pbf.get(variant));
            if (existing.is_some() || new.is_some()) && existing.and_then(|e| e.seq).is_none() {
                edit.set_pbf_seq(variant, seq).map_err(|e| e.to_string())?;
            }
        }
    }
    for (seq, (name, _, digest)) in &osc.files {
        let entry = FileEntry {
            file: name.clone(),
            xxhash: Some(digest.clone()),
            seq: None,
        };
        edit.set_osc(*seq, &entry).map_err(|e| e.to_string())?;
    }
    if new_raw.is_some() && primary.is_none_or(|g| g.download_date.is_none()) {
        edit.set_download_date(&req.today.iso())
            .map_err(|e| e.to_string())?;
    }
    for change in edit.commit().map_err(|e| e.to_string())? {
        note(&format!("  {change}"));
    }
    if new_raw.is_some() && osc.learned.is_none() {
        let entries =
            if new_indexed.is_some() || primary.is_some_and(|g| g.pbf.contains_key("indexed")) {
                "the pbf.raw and pbf.indexed entries"
            } else {
                "the pbf.raw entry"
            };
        note(&format!(
            "  NOTE: run 'pbfhogg inspect <file> --get header.replication.sequence' and add \
             seq = <N> to {entries}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct RefreshPrograms {
        upstream: Option<i64>,
        inner: SnapshotPrograms,
    }
    impl Programs for RefreshPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Ok(self.upstream)
        }
        fn inspect_get(&self, p: &Path, k: &str, l: &DatasetLock) -> Result<String, String> {
            self.inner.inspect_get(p, k, l)
        }
        fn fetch(&self, u: &str, p: &Path, l: &DatasetLock) -> Result<(), String> {
            self.inner.fetch(u, p, l)
        }
        fn cat(&self, p: &Path, o: &Path, l: &DatasetLock) -> Result<(), String> {
            self.inner.cat(p, o, l)
        }
    }

    #[test]
    fn refresh_checks_newness_recovers_and_rotates_the_whole_primary() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\ndownload_date = \"2020-01-01\"\n[h.datasets.denmark.pbf.raw]\nfile = \"old.osm.pbf\"\nseq = 9\n[h.datasets.denmark.osc.10]\nfile = \"old.osc.gz\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        std::fs::write(dir.join("old.osm.pbf"), b"old raw").unwrap();
        std::fs::write(dir.join("old.osc.gz"), b"old diff").unwrap();
        let config = Config::load(&path, None).unwrap();
        let mut tools = RefreshPrograms {
            upstream: None,
            inner: SnapshotPrograms {
                fail_cat: std::cell::Cell::new(true),
                fetches: std::cell::Cell::new(0),
            },
        };
        let today = CivilDate::parse_yyyymmdd("20260411").unwrap();
        for upstream in [None, Some(0), Some(1_577_836_800)] {
            tools.upstream = upstream;
            run(&Request {
                config: &config,
                host: "h",
                region: "denmark",
                refresh: true,
                force: false,
                snapshot: None,
                as_snapshot: None,
                osc_seq: None,
                indexed: true,
                tools: &tools,
                today,
            })
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            assert_eq!(tools.inner.fetches.get(), 0);
        }
        let req = Request {
            config: &config,
            host: "h",
            region: "denmark",
            refresh: true,
            force: true,
            snapshot: None,
            as_snapshot: None,
            osc_seq: Some(13),
            indexed: true,
            tools: &tools,
            today,
        };
        assert!(run(&req).unwrap_err().contains("injected cat failure"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        tools.inner.fail_cat.set(false);
        run(&req).unwrap();
        let updated = Config::load(&path, None).unwrap();
        let view = updated.dataset("h", "denmark").unwrap().unwrap();
        assert_eq!(view.snapshots["20200101"].pbf["raw"].seq, Some(9));
        assert!(view.snapshots["20200101"].osc.contains_key(&10));
        assert_eq!(view.primary.pbf["raw"].seq, Some(10));
        assert_eq!(view.primary.pbf["indexed"].seq, Some(10));
        assert_eq!(
            view.primary.osc.keys().copied().collect::<Vec<_>>(),
            [11, 12, 13]
        );
        assert_eq!(std::fs::read(dir.join("old.osm.pbf")).unwrap(), b"old raw");
        assert!(
            run(&Request {
                config: &updated,
                ..req
            })
            .unwrap_err()
            .contains("same-day")
        );
    }

    #[test]
    fn refresh_preflight_refuses_missing_raw_bad_pins_existing_archive_and_foreign_recovery() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let head = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\ndownload_date = \"2020-01-01\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(dir.join("old"), b"old").unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(false),
            fetches: std::cell::Cell::new(0),
        };
        for (tail, expected) in [
            ("", "requires pbf.raw"),
            (
                "[h.datasets.denmark.pbf.raw]\nfile = \"absent\"\n",
                "missing or empty",
            ),
            (
                "[h.datasets.denmark.pbf.raw]\nfile = \"old\"\nxxhash = \"00000000000000000000000000000000\"\n",
                "xxhash mismatch",
            ),
            (
                "[h.datasets.denmark.pbf.raw]\nfile = \"old\"\n[h.datasets.denmark.snapshot.20200101]\ndownload_date = \"2020-01-01\"\n",
                "already exists",
            ),
            (
                "[h.datasets.denmark.pbf.raw]\nfile = \"old\"\n",
                "was not published",
            ),
        ] {
            let before = format!("{head}{tail}");
            std::fs::write(&path, &before).unwrap();
            std::fs::write(dir.join("denmark-20260411.osm.pbf"), b"foreign").unwrap();
            let config = Config::load(&path, None).unwrap();
            let req = Request {
                config: &config,
                host: "h",
                region: "denmark",
                refresh: true,
                force: true,
                snapshot: None,
                as_snapshot: None,
                osc_seq: None,
                indexed: true,
                tools: &tools,
                today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
            };
            assert!(run(&req).unwrap_err().contains(expected), "{expected}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            assert_eq!(tools.fetches.get(), 0);
        }
    }

    #[test]
    fn refresh_archive_falls_back_to_raw_mtime() {
        let scratch = pbfhogg_test_scratch::scratch();
        let raw = scratch.path().join("raw");
        let file = std::fs::File::create(&raw).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                // fixture-time: UTC midnight on leap day 2000, not a wait bound.
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(951_782_400)),
        )
        .unwrap();
        for date in [None, Some("invalid"), Some("2026-02-29")] {
            let generation = Generation {
                download_date: date.map(str::to_owned),
                ..Generation::default()
            };
            let (day, seconds) = refresh_archive(&generation, &raw).unwrap();
            assert_eq!(day.iso(), "2000-02-29");
            assert_eq!(seconds, 951_782_400);
        }
    }

    #[test]
    fn refresh_refuses_host_wide_aliases_before_writing() {
        use std::os::unix::fs::symlink;
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(dir.join("old"), b"old").unwrap();
        // Another dataset registers the absent indexed final name through a
        // dangling alias: filling the name would change what it resolves to.
        symlink(
            dir.join("denmark-20260411-with-indexdata.osm.pbf"),
            dir.join("other"),
        )
        .unwrap();
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\ndownload_date = \"2020-01-01\"\n[h.datasets.denmark.pbf.raw]\nfile = \"old\"\n[h.datasets.other.snapshot.s.pbf.raw]\nfile = \"./other\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(false),
            fetches: std::cell::Cell::new(0),
        };
        let req = Request {
            config: &config,
            host: "h",
            region: "denmark",
            refresh: true,
            force: false,
            snapshot: None,
            as_snapshot: None,
            osc_seq: None,
            indexed: true,
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        assert!(run(&req).unwrap_err().contains("registered by 'other'"));
        assert_eq!(tools.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        pbfhogg_test_scratch::refused(
            dir.join("denmark-20260411-with-indexdata.osm.pbf")
                .symlink_metadata(),
            "No such file or directory",
        );
    }

    #[test]
    fn primary_and_snapshot_acquisition_never_write_another_datasets_file() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(dir.join("shared.osm.pbf"), b"denmark raw").unwrap();
        std::fs::write(dir.join("other"), b"protected").unwrap();
        // The other dataset registers Denmark's indexed name through an
        // alternate spelling of the data directory.
        let spelled = dir.join(".").join("shared-with-indexdata.osm.pbf");
        std::fs::rename(dir.join("other"), &spelled).unwrap();
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n[h.datasets.denmark.pbf.raw]\nfile = \"shared.osm.pbf\"\n[h.datasets.other.pbf.raw]\nfile = {:?}\n",
            dir.to_str().unwrap(),
            spelled.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(false),
            fetches: std::cell::Cell::new(0),
        };
        let req = Request {
            config: &config,
            host: "h",
            region: "denmark",
            refresh: false,
            force: false,
            snapshot: None,
            as_snapshot: None,
            osc_seq: None,
            indexed: true,
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        let err = run(&req).unwrap_err();
        assert!(err.contains("registered by 'other'"), "{err}");
        assert_eq!(std::fs::read(&spelled).unwrap(), b"protected");
        // A new snapshot refuses its coincident raw name before any transfer.
        let before =
            format!("{before}[h.datasets.other.polygon.p]\nfile = \"denmark-s.osm.pbf\"\n");
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let err = run(&Request {
            config: &config,
            as_snapshot: Some("s"),
            ..req
        })
        .unwrap_err();
        assert!(err.contains("registered by 'other'"), "{err}");
        assert_eq!(tools.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    /// The georgia sequence: two sources sharing one dataset key. A raw one
    /// source published and never registered is never adopted for the other.
    #[test]
    fn recovery_never_attributes_one_sources_bytes_to_another() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let head = format!("[h]\ndata = {:?}\n[h.datasets]\n", dir.to_str().unwrap());
        std::fs::write(&path, &head).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(true),
            fetches: std::cell::Cell::new(0),
        };
        let asia = Request {
            config: &config,
            host: "h",
            region: "asia/georgia",
            refresh: false,
            force: false,
            snapshot: None,
            as_snapshot: None,
            osc_seq: None,
            indexed: true,
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        // Day A: Asia publishes its raw and dies before registering.
        pbfhogg_test_scratch::refused(run(&asia), "injected cat failure");
        let leftover = dir.join("georgia-20260411.osm.pbf");
        assert!(leftover.is_file());
        // Day B: the US source registers under another name.
        tools.fail_cat.set(false);
        run(&Request {
            region: "north-america/us/georgia",
            today: CivilDate::parse_yyyymmdd("20260412").unwrap(),
            ..asia
        })
        .unwrap();
        let config = Config::load(&path, None).unwrap();
        let registered = std::fs::read_to_string(&path).unwrap();
        // A US snapshot at the leftover's name refuses rather than adopt it.
        let err = run(&Request {
            config: &config,
            region: "north-america/us/georgia",
            as_snapshot: Some("20260411"),
            ..asia
        })
        .unwrap_err();
        assert!(err.contains("was not published"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), registered);
    }

    /// Fails every transfer; cat writes a different indexed file each time.
    struct RegenPrograms(std::cell::Cell<u8>);
    impl Programs for RegenPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Err("unexpected HEAD".into())
        }
        fn inspect_get(&self, _: &Path, k: &str, _: &DatasetLock) -> Result<String, String> {
            Err(format!("unexpected lookup: {k}"))
        }
        fn fetch(&self, _: &str, _: &Path, _: &DatasetLock) -> Result<(), String> {
            Err("injected transfer failure".into())
        }
        fn cat(&self, _: &Path, staged: &Path, _: &DatasetLock) -> Result<(), String> {
            self.0.set(self.0.get() + 1);
            std::fs::write(staged, [self.0.get()]).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn only_an_indexed_file_this_tool_published_is_replaced() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(dir.join("d.osm.pbf"), b"raw").unwrap();
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n[h.datasets.denmark.pbf.raw]\nfile = \"d.osm.pbf\"\nseq = 10\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = RegenPrograms(std::cell::Cell::new(0));
        let req = Request {
            config: &config,
            host: "h",
            region: "denmark",
            refresh: false,
            force: false,
            snapshot: None,
            as_snapshot: None,
            osc_seq: Some(11),
            indexed: true,
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        let indexed = dir.join("d-with-indexdata.osm.pbf");
        for generation in [1u8, 2] {
            assert!(run(&req).unwrap_err().contains("injected transfer failure"));
            assert_eq!(std::fs::read(&indexed).unwrap(), [generation]);
        }
        // Bytes the journal does not vouch for are not replaced.
        std::fs::write(&indexed, b"hand placed").unwrap();
        assert!(run(&req).unwrap_err().contains("was not published"));
        assert_eq!(std::fs::read(&indexed).unwrap(), b"hand placed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn staging_never_unlinks_a_registered_entry() {
        use std::os::unix::fs::symlink;
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let staged = dir.join(".partial.denmark.denmark-new.osm.pbf");
        std::fs::create_dir(dir.join("store")).unwrap();
        std::fs::write(dir.join("store/raw"), b"protected").unwrap();
        let head = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n",
            dir.to_str().unwrap()
        );
        // The staging name registered as a file, and traversed as a
        // symlinked directory by another dataset's registration.
        for (file, setup) in [
            (".partial.denmark.denmark-new.osm.pbf", false),
            (".partial.denmark.denmark-new.osm.pbf/raw", true),
        ] {
            if staged.symlink_metadata().is_ok() {
                std::fs::remove_file(&staged).unwrap();
            }
            if setup {
                symlink(dir.join("store"), &staged).unwrap();
            } else {
                std::fs::write(&staged, b"protected").unwrap();
            }
            let before = format!("{head}[h.datasets.other.pbf.raw]\nfile = {file:?}\n");
            std::fs::write(&path, &before).unwrap();
            let config = Config::load(&path, None).unwrap();
            let tools = SnapshotPrograms {
                fail_cat: std::cell::Cell::new(false),
                fetches: std::cell::Cell::new(0),
            };
            let req = Request {
                config: &config,
                host: "h",
                region: "denmark",
                refresh: false,
                force: false,
                snapshot: None,
                as_snapshot: Some("new"),
                osc_seq: None,
                indexed: false,
                tools: &tools,
                today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
            };
            let err = run(&req).unwrap_err();
            assert!(err.contains("registered by 'other'"), "{err}");
            assert_eq!(tools.fetches.get(), 0);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            assert_eq!(std::fs::read(dir.join(file)).unwrap(), b"protected");
        }
    }

    #[test]
    fn continuity_and_holes() {
        let regional = Source::Geofabrik("europe/denmark".into());
        for (source, configured, header, expected) in [
            (&regional, Some(10), None, Some((10, false))),
            (
                &regional,
                Some(10),
                Some(("99", "wrong")),
                Some((10, false)),
            ),
            (
                &regional,
                None,
                Some((
                    "10",
                    "https://download.geofabrik.de/europe/denmark-updates/",
                )),
                Some((10, true)),
            ),
            (&regional, None, Some(("10", "wrong")), None),
            (
                &regional,
                None,
                Some((
                    "bad",
                    "https://download.geofabrik.de/europe/denmark-updates",
                )),
                None,
            ),
            (
                &Source::Planet,
                None,
                Some(("10", "https://planet.openstreetmap.org/replication/day")),
                None,
            ),
            (&Source::Planet, Some(10), None, Some((10, false))),
        ] {
            assert_eq!(base_seq(source, configured, header).ok(), expected);
        }
        let mut g = Generation::default();
        g.osc.insert(
            12,
            FileEntry {
                file: "x".into(),
                xxhash: None,
                seq: None,
            },
        );
        assert_eq!(missing_seqs(&g, 10, 13).collect::<Vec<_>>(), [11, 13]);
        assert!(missing_seqs(&g, 13, 10).next().is_none());
        assert!(missing_seqs(&g, u64::MAX, u64::MAX).next().is_none());
    }

    struct LocalPrograms;

    impl Programs for LocalPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Err("unexpected HEAD".into())
        }
        fn inspect_get(&self, _: &Path, key: &str, _: &DatasetLock) -> Result<String, String> {
            match key {
                "header.replication.sequence" => Ok("10".into()),
                "header.replication.url" => {
                    Ok("https://download.geofabrik.de/europe/denmark-updates/".into())
                }
                _ => Err(format!("unexpected lookup: {key}")),
            }
        }

        fn fetch(&self, url: &str, staged: &Path, _: &DatasetLock) -> Result<(), String> {
            if url != "https://download.geofabrik.de/europe/denmark-updates/000/000/013.osc.gz" {
                return Err(format!("unexpected transfer: {url}"));
            }
            std::fs::write(staged, b"fixture").map_err(|e| e.to_string())
        }

        fn cat(&self, _: &Path, _: &Path, _: &DatasetLock) -> Result<(), String> {
            Err("unexpected indexed generation".into())
        }
    }

    /// Transfers succeed; indexed generation is a test failure.
    struct RawOnlyPrograms {
        cli: bool,
        fetches: std::cell::Cell<usize>,
    }
    impl Programs for RawOnlyPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Err("unexpected HEAD".into())
        }
        fn inspect_get(&self, p: &Path, k: &str, l: &DatasetLock) -> Result<String, String> {
            assert!(self.cli, "inspect without a CLI");
            LocalPrograms.inspect_get(p, k, l)
        }
        fn fetch(&self, _: &str, staged: &Path, _: &DatasetLock) -> Result<(), String> {
            self.fetches.set(self.fetches.get() + 1);
            std::fs::write(staged, b"raw only").map_err(|e| e.to_string())
        }
        fn cat(&self, _: &Path, _: &Path, _: &DatasetLock) -> Result<(), String> {
            Err("unexpected indexed generation".into())
        }
        fn has_cli(&self) -> bool {
            self.cli
        }
    }

    #[test]
    fn no_indexed_registers_raw_only_and_needs_a_cli_only_to_learn_seq() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let before = format!("[h]\ndata = {:?}\n[h.datasets]\n", dir.to_str().unwrap());
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let no_cli = RawOnlyPrograms {
            cli: false,
            fetches: std::cell::Cell::new(0),
        };
        let req = Request {
            config: &config,
            host: "h",
            region: "denmark",
            osc_seq: Some(11),
            refresh: false,
            force: false,
            as_snapshot: None,
            snapshot: None,
            indexed: false,
            tools: &no_cli,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        // Header seq learning needs the CLI: refused before any transfer.
        assert!(run(&req).unwrap_err().contains("--pbfhogg"));
        assert_eq!(no_cli.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let cli = RawOnlyPrograms {
            cli: true,
            fetches: std::cell::Cell::new(0),
        };
        run(&Request { tools: &cli, ..req }).unwrap();
        assert_eq!(cli.fetches.get(), 2);
        let config = Config::load(&path, None).unwrap();
        let view = config.dataset("h", "denmark").unwrap().unwrap();
        assert_eq!(
            view.primary
                .pbf
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["raw"]
        );
        assert_eq!(view.primary.pbf["raw"].file, "denmark-20260411.osm.pbf");
        assert_eq!(view.primary.pbf["raw"].seq, Some(10));
        assert_eq!(view.primary.osc.keys().copied().collect::<Vec<_>>(), [11]);
        assert!(!dir.join("denmark-20260411-with-indexdata.osm.pbf").exists());

        // Without a CLI and without --osc-seq, a raw-only snapshot registers.
        run(&Request {
            config: &config,
            osc_seq: None,
            as_snapshot: Some("20260101"),
            ..req
        })
        .unwrap();
        let config = Config::load(&path, None).unwrap();
        let view = config.dataset("h", "denmark").unwrap().unwrap();
        assert_eq!(
            view.snapshots["20260101"]
                .pbf
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["raw"]
        );
    }

    #[test]
    fn no_indexed_refuses_an_indexed_only_primary() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(dir.join("i.osm.pbf"), b"indexed").unwrap();
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n[h.datasets.denmark.pbf.indexed]\nfile = \"i.osm.pbf\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = RawOnlyPrograms {
            cli: false,
            fetches: std::cell::Cell::new(0),
        };
        let err = run(&Request {
            config: &config,
            host: "h",
            region: "denmark",
            osc_seq: None,
            refresh: false,
            force: false,
            as_snapshot: None,
            snapshot: None,
            indexed: false,
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        })
        .unwrap_err();
        assert!(err.contains("no pbf.raw"), "{err}");
        assert_eq!(tools.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn roll_forward_adopts_fills_holes_and_commits_once() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let config_path = dir.join("brokkr.toml");
        std::fs::write(&config_path, format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n[h.datasets.denmark.pbf.raw]\nfile = \"raw.osm.pbf\"\n[h.datasets.denmark.pbf.indexed]\nfile = \"indexed.osm.pbf\"\n[h.datasets.denmark.osc.12]\nfile = \"registered.osc.gz\"\n", dir.to_str().unwrap())).unwrap();
        for name in [
            "raw.osm.pbf",
            "indexed.osm.pbf",
            "registered.osc.gz",
            "denmark-seq11.osc.gz",
        ] {
            std::fs::write(dir.join(name), b"fixture").unwrap();
        }
        let digest = file_digest(&dir.join("denmark-seq11.osc.gz")).unwrap();
        let journal = dir.join(".denmark.download.journal");
        let record = |origin: &str| {
            std::fs::write(
                &journal,
                format!("denmark-seq11.osc.gz\tosc\t{origin}\t{digest}\n"),
            )
            .unwrap();
        };
        let tools = LocalPrograms;
        let config = Config::load(&config_path, None).unwrap();
        let req = Request {
            refresh: false,
            force: false,
            config: &config,
            host: "h",
            region: "denmark",
            osc_seq: Some(13),
            snapshot: None,
            as_snapshot: None,
            indexed: true,
            tools: &tools,
            today: CivilDate::today(),
        };
        let before = std::fs::read_to_string(&config_path).unwrap();
        // A leftover recorded for another source, or with no record at all
        // (the legacy single-origin lock content), is not adopted.
        for wrong in [Some("wrong source"), None] {
            match wrong {
                Some(origin) => record(origin),
                None => std::fs::write(&journal, "download.geofabrik.de/europe/denmark\n").unwrap(),
            }
            assert!(run(&req).unwrap_err().contains("was not published"));
            assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
        }
        record("download.geofabrik.de/europe/denmark");
        // A later transfer failure must leave both adoption and learned seq
        // unregistered, even though seq 13 was already published.
        pbfhogg_test_scratch::refused(
            run(&Request {
                osc_seq: Some(14),
                ..req
            }),
            "unexpected transfer",
        );
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
        assert!(dir.join("denmark-seq13.osc.gz").is_file());
        run(&req).unwrap();
        let config = Config::load(&config_path, None).unwrap();
        let g = config.dataset("h", "denmark").unwrap().unwrap().primary;
        assert_eq!(g.osc.keys().copied().collect::<Vec<_>>(), [11, 12, 13]);
        assert_eq!(g.pbf["raw"].seq, Some(10));
        assert_eq!(g.pbf["indexed"].seq, Some(10));
        assert!(g.osc[&11].xxhash.is_some());
        run(&Request {
            refresh: false,
            force: false,
            config: &config,
            ..req
        })
        .unwrap();
        std::fs::write(dir.join("denmark-seq11.osc.gz"), b"changed").unwrap();
        pbfhogg_test_scratch::refused(
            run(&Request {
                refresh: false,
                force: false,
                config: &config,
                ..req
            }),
            "denmark-seq11.osc.gz: xxhash mismatch",
        );
    }

    struct SnapshotPrograms {
        fail_cat: std::cell::Cell<bool>,
        fetches: std::cell::Cell<usize>,
    }

    impl Programs for SnapshotPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Ok(Some(1_775_865_600))
        }
        fn inspect_get(
            &self,
            path: &Path,
            key: &str,
            lock: &DatasetLock,
        ) -> Result<String, String> {
            assert_eq!(
                std::fs::read(path).map_err(|e| e.to_string())?,
                b"snapshot raw"
            );
            LocalPrograms.inspect_get(path, key, lock)
        }
        fn fetch(&self, url: &str, staged: &Path, _: &DatasetLock) -> Result<(), String> {
            self.fetches.set(self.fetches.get() + 1);
            let bytes: &[u8] = if url.ends_with(".osm.pbf") {
                b"snapshot raw"
            } else {
                b"diff"
            };
            std::fs::write(staged, bytes).map_err(|e| e.to_string())
        }
        fn cat(&self, _: &Path, staged: &Path, _: &DatasetLock) -> Result<(), String> {
            if self.fail_cat.get() {
                return Err("injected cat failure".into());
            }
            std::fs::write(staged, b"new indexed").map_err(|e| e.to_string())
        }
    }

    #[test]
    fn snapshot_recovers_atomically_and_uses_its_own_sequence() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\ndownload_date = \"2020-01-01\"\n[h.datasets.denmark.pbf.raw]\nfile = \"primary.osm.pbf\"\nseq = 999\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(true),
            fetches: std::cell::Cell::new(0),
        };
        let req = Request {
            refresh: false,
            force: false,
            config: &config,
            host: "h",
            region: "denmark",
            osc_seq: Some(13),
            snapshot: None,
            as_snapshot: Some("20260411"),
            indexed: true,
            tools: &tools,
            today: CivilDate::from_unix_timestamp(0).unwrap(),
        };
        for key in ["", "base", "../bad", "a.b", "non ascii"] {
            pbfhogg_test_scratch::refused(
                run(&Request {
                    as_snapshot: Some(key),
                    ..req
                }),
                "snapshot key must match",
            );
        }
        pbfhogg_test_scratch::refused(
            run(&Request {
                region: "malta",
                ..req
            }),
            "not found; download its primary first",
        );
        assert_eq!(tools.fetches.get(), 0);
        pbfhogg_test_scratch::refused(run(&req), "injected cat failure");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(tools.fetches.get(), 1);
        // Recovery adopts the published raw, but an indexed file this tool
        // never published at the indexed name is refused, not overwritten.
        let stale = dir.join("denmark-20260411-with-indexdata.osm.pbf");
        std::fs::write(&stale, b"stale").unwrap();
        tools.fail_cat.set(false);
        assert!(run(&req).unwrap_err().contains("was not published"));
        assert_eq!(std::fs::read(&stale).unwrap(), b"stale");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_file(&stale).unwrap();
        run(&req).unwrap();
        assert_eq!(tools.fetches.get(), 4);
        let updated = Config::load(&path, None).unwrap();
        let view = updated.dataset("h", "denmark").unwrap().unwrap();
        assert_eq!(view.primary.pbf["raw"].seq, Some(999));
        assert_eq!(view.primary.download_date.as_deref(), Some("2020-01-01"));
        let snap = &view.snapshots["20260411"];
        assert_eq!(snap.download_date.as_deref(), Some("2026-04-11"));
        assert_eq!(snap.osc.keys().copied().collect::<Vec<_>>(), [11, 12, 13]);
        for variant in ["raw", "indexed"] {
            assert_eq!(snap.pbf[variant].seq, Some(10));
            assert!(snap.pbf[variant].xxhash.is_some());
        }
        assert_eq!(
            std::fs::read(dir.join(&snap.pbf["indexed"].file)).unwrap(),
            b"new indexed"
        );
        pbfhogg_test_scratch::refused(
            run(&Request {
                config: &updated,
                ..req
            }),
            "snapshot '20260411' is already registered",
        );
        assert_eq!(tools.fetches.get(), 4);
        // Non-date keys use today's date and need no header or OSC lookup.
        run(&Request {
            config: &updated,
            snapshot: None,
            as_snapshot: Some("named-key"),
            osc_seq: None,
            ..req
        })
        .unwrap();
        let updated = Config::load(&path, None).unwrap();
        assert_eq!(
            updated.dataset("h", "denmark").unwrap().unwrap().snapshots["named-key"]
                .download_date
                .as_deref(),
            Some("1970-01-01")
        );
    }

    struct ExtensionPrograms(SnapshotPrograms);

    impl Programs for ExtensionPrograms {
        fn head(&self, _: &str, _: &DatasetLock) -> Result<Option<i64>, String> {
            Err("unexpected HEAD".into())
        }
        fn inspect_get(&self, raw: &Path, key: &str, lock: &DatasetLock) -> Result<String, String> {
            LocalPrograms.inspect_get(raw, key, lock)
        }
        fn cat(&self, _: &Path, _: &Path, _: &DatasetLock) -> Result<(), String> {
            Err("unexpected cat".into())
        }
        fn fetch(&self, url: &str, staged: &Path, lock: &DatasetLock) -> Result<(), String> {
            if url.ends_with("014.osc.gz") {
                return Err("injected transfer failure".into());
            }
            self.0.fetch(url, staged, lock)
        }
    }

    #[test]
    fn existing_snapshot_extends_with_shared_diffs_and_atomic_retry() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.planet]\norigin = \"planet.openstreetmap.org\"\ndownload_date = \"2020-01-01\"\n[h.datasets.planet.pbf.raw]\nfile = \"primary\"\nseq = 999\n[h.datasets.planet.osc.11]\nfile = \"shared\"\n[h.datasets.planet.snapshot.s]\ndownload_date = \"2019-01-01\"\n[h.datasets.planet.snapshot.s.pbf.raw]\nfile = \"raw\"\nseq = 10\n[h.datasets.planet.snapshot.s.osc.12]\nfile = \"existing\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(&path, &before).unwrap();
        for name in ["raw", "shared", "existing"] {
            std::fs::write(dir.join(name), b"fixture").unwrap();
        }
        // A shared registered diff needs no recovery record, even if it
        // occupies the standard dataset-wide destination name.
        std::fs::rename(dir.join("shared"), dir.join("planet-seq11.osc.gz")).unwrap();
        let before = before.replace("file = \"shared\"", "file = \"planet-seq11.osc.gz\"");
        std::fs::write(&path, &before).unwrap();
        let tools = ExtensionPrograms(SnapshotPrograms {
            fail_cat: std::cell::Cell::new(true),
            fetches: std::cell::Cell::new(0),
        });
        let config = Config::load(&path, None).unwrap();
        let req = Request {
            config: &config,
            host: "h",
            region: "planet",
            osc_seq: Some(13),
            snapshot: Some("s"),
            as_snapshot: None,
            refresh: false,
            force: false,
            indexed: true,
            tools: &tools,
            today: CivilDate::today(),
        };
        // Shared pins are checked, never replaced by a fresh transfer.
        std::fs::remove_file(dir.join("planet-seq11.osc.gz")).unwrap();
        assert!(run(&req).unwrap_err().contains("missing or empty"));
        assert_eq!(tools.0.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::write(dir.join("planet-seq11.osc.gz"), b"fixture").unwrap();
        assert!(
            run(&Request {
                osc_seq: Some(14),
                ..req
            })
            .unwrap_err()
            .contains("injected transfer failure")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(dir.join("planet-seq13.osc.gz").is_file());
        run(&req).unwrap();
        assert_eq!(tools.0.fetches.get(), 1);
        let updated = Config::load(&path, None).unwrap();
        let view = updated.dataset("h", "planet").unwrap().unwrap();
        assert_eq!(view.primary.pbf["raw"].seq, Some(999));
        assert_eq!(view.primary.osc.len(), 1);
        assert_eq!(
            view.snapshots["s"].download_date.as_deref(),
            Some("2019-01-01")
        );
        assert_eq!(
            view.snapshots["s"].osc.keys().copied().collect::<Vec<_>>(),
            [11, 12, 13]
        );
        assert_eq!(view.snapshots["s"].osc[&11].file, "planet-seq11.osc.gz");
        run(&Request {
            config: &updated,
            ..req
        })
        .unwrap();
        assert_eq!(tools.0.fetches.get(), 1);
        assert!(
            run(&Request {
                snapshot: Some("missing"),
                ..req
            })
            .unwrap_err()
            .contains("not registered")
        );
        // Planet must not borrow the primary's day seq when its own is absent.
        std::fs::write(&path, before.replace("seq = 10\n", "")).unwrap();
        let no_seq = Config::load(&path, None).unwrap();
        assert!(
            run(&Request {
                config: &no_seq,
                ..req
            })
            .unwrap_err()
            .contains("needs seq")
        );
        assert_eq!(tools.0.fetches.get(), 1);
    }

    #[test]
    fn existing_regional_snapshot_learns_its_own_header_sequence() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        std::fs::write(&path, format!(
            "[h]\ndata = {:?}\n[h.datasets.denmark]\norigin = \"download.geofabrik.de/europe/denmark\"\n[h.datasets.denmark.pbf.raw]\nfile = \"primary\"\nseq = 999\n[h.datasets.denmark.snapshot.s.pbf.raw]\nfile = \"raw\"\n[h.datasets.denmark.snapshot.s.pbf.indexed]\nfile = \"raw\"\n", dir.to_str().unwrap()
        )).unwrap();
        std::fs::write(dir.join("raw"), b"fixture").unwrap();
        let tools = ExtensionPrograms(SnapshotPrograms {
            fail_cat: std::cell::Cell::new(true),
            fetches: std::cell::Cell::new(0),
        });
        let config = Config::load(&path, None).unwrap();
        run(&Request {
            config: &config,
            host: "h",
            region: "denmark",
            osc_seq: Some(11),
            snapshot: Some("s"),
            as_snapshot: None,
            refresh: false,
            force: false,
            indexed: true,
            tools: &tools,
            today: CivilDate::today(),
        })
        .unwrap();
        let config = Config::load(&path, None).unwrap();
        let view = config.dataset("h", "denmark").unwrap().unwrap();
        for variant in ["raw", "indexed"] {
            assert_eq!(view.snapshots["s"].pbf[variant].seq, Some(10));
        }
        assert_eq!(view.primary.pbf["raw"].seq, Some(999));
        assert_eq!(tools.0.fetches.get(), 1);
    }

    #[test]
    fn shared_copies_must_agree_and_pass_every_pin() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let path = dir.join("brokkr.toml");
        let before = format!(
            "[h]\ndata = {:?}\n[h.datasets.planet]\norigin = \"planet.openstreetmap.org\"\n[h.datasets.planet.pbf.raw]\nfile = \"raw\"\nseq = 10\n[h.datasets.planet.snapshot.a.osc.11]\nfile = \"a\"\n[h.datasets.planet.snapshot.b.osc.11]\nfile = \"b\"\n",
            dir.to_str().unwrap()
        );
        for name in ["raw", "a"] {
            std::fs::write(dir.join(name), b"fixture").unwrap();
        }
        std::fs::write(dir.join("b"), b"different").unwrap();
        std::fs::write(&path, &before).unwrap();
        let tools = SnapshotPrograms {
            fail_cat: std::cell::Cell::new(false),
            fetches: std::cell::Cell::new(0),
        };
        // Primary indexed generation is intentionally already registered.
        let before = before.replace("[h.datasets.planet.snapshot.a.osc.11]", "[h.datasets.planet.pbf.indexed]\nfile = \"raw\"\n[h.datasets.planet.snapshot.a.osc.11]");
        std::fs::write(&path, &before).unwrap();
        let config = Config::load(&path, None).unwrap();
        let req = Request {
            config: &config,
            host: "h",
            region: "planet",
            osc_seq: Some(11),
            snapshot: None,
            as_snapshot: None,
            refresh: false,
            force: false,
            indexed: true,
            tools: &tools,
            today: CivilDate::today(),
        };
        assert!(
            run(&req)
                .unwrap_err()
                .contains("conflicting registered copies")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let bad_pin = before.replace(
            "file = \"b\"",
            "file = \"b\"\nxxhash = \"00000000000000000000000000000000\"",
        );
        std::fs::write(&path, bad_pin).unwrap();
        let config = Config::load(&path, None).unwrap();
        assert!(
            run(&Request {
                config: &config,
                ..req
            })
            .unwrap_err()
            .contains("xxhash mismatch")
        );
        assert_eq!(tools.fetches.get(), 0);
    }

    #[test]
    fn indexed_names_pair_with_their_raw() {
        let today = CivilDate::from_unix_timestamp(0).unwrap_or_else(CivilDate::today);
        assert_eq!(
            indexed_name_for("denmark-20260220-seq4704.osm.pbf", "denmark", today),
            "denmark-20260220-seq4704-with-indexdata.osm.pbf"
        );
        assert_eq!(
            indexed_name_for("odd.pbf", "d", today),
            "d-19700101-with-indexdata.osm.pbf"
        );
    }
}
