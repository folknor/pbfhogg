//! `download REGION`: acquire a dataset's primary PBF, generate its indexed
//! variant, and register both.
//!
//! Registration follows the desired state, not this run's work log: every
//! file the dataset should have and does not register is produced or, when
//! a complete file already sits at its target name from a run that died
//! before registering, adopted. A raw PBF is adopted only under a matching
//! source identity (`lock`); an unregistered indexed file is never adopted,
//! always regenerated from the raw being registered. A registered entry
//! whose file is missing, or fails its pin, refuses: the tool never rewrites
//! a pin.

use std::path::{Path, PathBuf};

use pbfhogg_devkit::{Config, DatasetView, FileEntry, Generation, file_digest};

use crate::date::CivilDate;
use crate::layout::{
    clear, indexed_pbf_filename, osc_filename, partial, publication_path, publish, raw_pbf_filename,
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

/// Whether any entry of the dataset names `file`.
fn names(view: Option<&DatasetView>, file: &str) -> bool {
    view.is_some_and(|v| {
        std::iter::once(&v.primary)
            .chain(v.snapshots.values())
            .any(|g| g.pbf.values().chain(g.osc.values()).any(|e| e.file == file))
    })
}

/// Bind the lock's recorded identity to `source`. A file this run would
/// adopt (`recoverable`) is only adopted under an identity already equal to
/// the request; the identity is only (re)written when nothing is adoptable.
fn bind_identity(
    lock: &DatasetLock,
    source: &Source,
    recoverable: Option<&Path>,
) -> Result<(), String> {
    let want = source.origin();
    let have = lock.identity()?;
    match recoverable {
        Some(file) if have.as_deref() != Some(want.as_str()) => Err(format!(
            "{} is not registered and was not downloaded from {want} by this tool; \
             remove it and retry",
            file.display()
        )),
        Some(_) => Ok(()),
        None if have.as_deref() == Some(want.as_str()) => Ok(()),
        None => lock.set_identity(&want),
    }
}

/// A file this run produced or adopted, to be registered.
type New = Option<(String, PathBuf)>;

/// Fetch (or adopt) the primary raw PBF of a dataset that registers no PBF.
fn acquire_raw(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    data_dir: &Path,
    lock: &DatasetLock,
    suffix: &str,
) -> Result<(String, PathBuf), String> {
    let key = resolved.key.as_str();
    let name = raw_pbf_filename(key, suffix);
    let dest = data_dir.join(&name);
    if names(resolved.dataset.as_ref(), &name) {
        return Err(format!(
            "{name} is already named by another entry of '{key}'"
        ));
    }
    let adopt = nonempty(&dest);
    bind_identity(lock, &resolved.source, adopt.then_some(dest.as_path()))?;
    if adopt {
        note(&format!(
            "  ADOPT (complete, unregistered): {}",
            dest.display()
        ));
    } else {
        let url = resolved.source.pbf_url();
        let staged = partial(&dest);
        clear(&staged)?;
        note(&format!("  GET: {url} -> {}", dest.display()));
        req.tools.fetch(&url, &staged, lock)?;
        publish(&staged, &dest)?;
    }
    Ok((name, dest))
}

/// Generate the indexed variant of `raw` with the handed pbfhogg binary.
fn generate_indexed(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    raw: &Path,
    data_dir: &Path,
    lock: &DatasetLock,
) -> Result<(String, PathBuf), String> {
    let key = resolved.key.as_str();
    let raw_name = raw.file_name().unwrap_or_default().to_string_lossy();
    let name = indexed_name_for(&raw_name, key, req.today);
    if names(resolved.dataset.as_ref(), &name) {
        return Err(format!(
            "{name} is already named by another entry of '{key}'"
        ));
    }
    let dest = data_dir.join(&name);
    let staged = partial(&dest);
    clear(&staged)?;
    note(&format!(
        "  generating indexed PBF via pbfhogg cat -> {}",
        dest.display()
    ));
    req.tools.cat(raw, &staged, lock)?;
    publish(&staged, &dest)?;
    Ok((name, dest))
}

#[derive(Default)]
struct Osc {
    learned: Option<u64>,
    files: Vec<(u64, String, PathBuf)>,
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

fn bind_osc_identity(
    lock: &DatasetLock,
    source: &Source,
    dir: &Path,
    key: &str,
    generation: Option<&Generation>,
    view: Option<&DatasetView>,
    target: u64,
) -> Result<(), String> {
    let prefix = format!("{key}-seq");
    let base = generation
        .and_then(|g| g.pbf.get("raw"))
        .and_then(|e| e.seq);
    for item in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let name = item.file_name();
        let Some(seq) = name
            .to_str()
            .and_then(|n| n.strip_prefix(&prefix))
            .and_then(|n| n.strip_suffix(".osc.gz"))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if seq <= target
            && base.is_none_or(|base| seq > base)
            && generation.is_none_or(|g| !g.osc.contains_key(&seq))
            && !names(view, &name.to_string_lossy())
            && nonempty(&item.path())
        {
            return bind_identity(lock, source, Some(&item.path()));
        }
    }
    // Do not rewrite here: acquire_raw must first check recoverable raw.
    Ok(())
}

fn acquire_osc(
    req: &Request<'_>,
    resolved: &crate::source::Resolved,
    raw: &Path,
    dir: &Path,
    lock: &DatasetLock,
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
            if let Some((name, path, _)) = shared {
                osc.files.push((seq, name, path));
                continue;
            }
        }
        let name = osc_filename(&resolved.key, seq);
        if names(resolved.dataset.as_ref(), &name) {
            return Err(format!("{name} is already named by another entry"));
        }
        publication_destinations(req, dir, std::slice::from_ref(&name))?;
        let dest = dir.join(&name);
        let adopt = nonempty(&dest);
        bind_identity(lock, &resolved.source, adopt.then_some(dest.as_path()))?;
        if adopt {
            note(&format!(
                "  ADOPT (complete, unregistered): {}",
                dest.display()
            ));
        } else {
            let staged = partial(&dest);
            clear(&staged)?;
            let url = resolved.source.osc_url(seq);
            note(&format!("  GET: {url} -> {}", dest.display()));
            req.tools.fetch(&url, &staged, lock)?;
            publish(&staged, &dest)?;
        }
        osc.files.push((seq, name, dest));
    }
    Ok(osc)
}

fn publication_destinations(req: &Request<'_>, dir: &Path, names: &[String]) -> Result<(), String> {
    let targets = names
        .iter()
        .map(|n| publication_path(&dir.join(n)))
        .collect::<Result<Vec<_>, _>>()?;
    for (key, view) in req
        .config
        .dataset_views(req.host)
        .map_err(|e| e.to_string())?
    {
        for generation in std::iter::once(&view.primary).chain(view.snapshots.values()) {
            for entry in generation.pbf.values().chain(generation.osc.values()) {
                let path = publication_path(&dir.join(&entry.file))?;
                if targets.contains(&path) {
                    return Err(format!(
                        "publication would overwrite {} registered by '{key}'",
                        path.display()
                    ));
                }
            }
        }
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
    publication_destinations(
        req,
        dir,
        &[
            raw_pbf_filename(&resolved.key, &suffix),
            indexed_pbf_filename(&resolved.key, &suffix),
        ],
    )?;
    if let Some(target) = req.osc_seq {
        bind_osc_identity(
            lock,
            &resolved.source,
            dir,
            &resolved.key,
            None,
            resolved.dataset.as_ref(),
            target,
        )?;
    }
    let (raw_name, raw) = acquire_raw(req, resolved, dir, lock, &suffix)?;
    let (indexed_name, indexed) = generate_indexed(req, resolved, &raw, dir, lock)?;
    let osc = if let Some(target) = req.osc_seq {
        acquire_osc(
            req,
            resolved,
            &raw,
            dir,
            lock,
            target,
            &Generation::default(),
        )?
    } else {
        Osc::default()
    };
    let entries = [("raw", raw_name, raw), ("indexed", indexed_name, indexed)]
        .into_iter()
        .map(|(variant, file, path)| {
            Ok((
                variant,
                FileEntry {
                    file,
                    xxhash: Some(file_digest(&path).map_err(|e| e.to_string())?),
                    seq: osc.learned,
                },
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut edit = req
        .config
        .edit_dataset(req.host, &resolved.key)
        .map_err(|e| e.to_string())?;
    edit.rotate_primary_to_snapshot(&snap, &archive_date.iso(), &req.today.iso())
        .map_err(|e| e.to_string())?;
    for (variant, entry) in entries {
        edit.set_pbf(variant, &entry).map_err(|e| e.to_string())?;
    }
    for (seq, file, path) in osc.files {
        edit.set_osc(
            seq,
            &FileEntry {
                file,
                xxhash: Some(file_digest(&path).map_err(|e| e.to_string())?),
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
    let lock = DatasetLock::acquire(&data_dir, key)?;
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
    }
    // Bind before any producer can rewrite the identity. Include all
    // potentially adoptable diffs, even before the raw header gives a base.
    if let Some(target) = req.osc_seq {
        bind_osc_identity(
            &lock,
            &resolved.source,
            &data_dir,
            key,
            primary,
            resolved.dataset.as_ref(),
            target,
        )?;
    }
    let mut new_raw: New = None;
    let raw_path = if let Some(entry) = raw_entry {
        note(&format!("  SKIP (registered): {}", entry.file));
        Some(registered(&data_dir, entry, "pbf.raw")?)
    } else if indexed_entry.is_some() {
        // An indexed-only dataset has nothing to derive and nothing to fetch.
        None
    } else {
        let (name, dest) = acquire_raw(req, &resolved, &data_dir, &lock, &req.today.yyyymmdd())?;
        new_raw = Some((name, dest.clone()));
        Some(dest)
    };
    let new_indexed: New = match (indexed_entry, &raw_path) {
        (None, Some(raw)) => Some(generate_indexed(req, &resolved, raw, &data_dir, &lock)?),
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
    bind_osc_identity(
        lock,
        &resolved.source,
        dir,
        &resolved.key,
        Some(generation),
        resolved.dataset.as_ref(),
        target,
    )?;
    let osc = acquire_osc(req, resolved, &raw, dir, lock, target, generation)?;
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
    for (seq, file, path) in osc.files {
        edit.set_snapshot_osc(
            snap,
            seq,
            &FileEntry {
                file,
                xxhash: Some(file_digest(&path).map_err(|e| e.to_string())?),
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
    // Protect all recoverable diffs before acquire_raw can bind the identity.
    if let Some(target) = req.osc_seq {
        bind_osc_identity(
            lock,
            &resolved.source,
            dir,
            &resolved.key,
            Some(&generation),
            resolved.dataset.as_ref(),
            target,
        )?;
    }
    // Check both output names before any producer runs.
    for name in [
        raw_pbf_filename(&resolved.key, snap),
        indexed_pbf_filename(&resolved.key, snap),
    ] {
        if names(resolved.dataset.as_ref(), &name) {
            return Err(format!("{name} is already named by another entry"));
        }
    }
    let (raw_name, raw) = acquire_raw(req, resolved, dir, lock, snap)?;
    let (indexed_name, indexed) = generate_indexed(req, resolved, &raw, dir, lock)?;
    let osc = match req.osc_seq {
        Some(target) => acquire_osc(req, resolved, &raw, dir, lock, target, &generation)?,
        None => Osc::default(),
    };
    let mut edit = req
        .config
        .edit_dataset(req.host, &resolved.key)
        .map_err(|e| e.to_string())?;
    let date = CivilDate::parse_yyyymmdd(snap).unwrap_or(req.today);
    edit.set_snapshot_header(snap, &date.iso())
        .map_err(|e| e.to_string())?;
    for (variant, name, path) in [("raw", raw_name, raw), ("indexed", indexed_name, indexed)] {
        let entry = FileEntry {
            file: name,
            xxhash: Some(file_digest(&path).map_err(|e| e.to_string())?),
            seq: osc.learned,
        };
        edit.set_snapshot_pbf(snap, variant, &entry)
            .map_err(|e| e.to_string())?;
    }
    for (seq, name, path) in osc.files {
        let entry = FileEntry {
            file: name,
            xxhash: Some(file_digest(&path).map_err(|e| e.to_string())?),
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
        if let Some((name, path)) = new {
            note(&format!("  hashing {name}..."));
            let xxhash = file_digest(path).map_err(|e| e.to_string())?;
            let entry = FileEntry {
                file: name.clone(),
                xxhash: Some(xxhash),
                seq: None,
            };
            edit.set_pbf(variant, &entry).map_err(|e| e.to_string())?;
        }
    }
    if let Some(seq) = osc.learned {
        for variant in ["raw", "indexed"] {
            if primary
                .and_then(|g| g.pbf.get(variant))
                .and_then(|e| e.seq)
                .is_none()
            {
                edit.set_pbf_seq(variant, seq).map_err(|e| e.to_string())?;
            }
        }
    }
    for (seq, name, path) in &osc.files {
        let entry = FileEntry {
            file: name.clone(),
            xxhash: Some(file_digest(path).map_err(|e| e.to_string())?),
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
        note(
            "  NOTE: run 'pbfhogg inspect <file> --get header.replication.sequence' and add \
             seq = <N> to the pbf.raw and pbf.indexed entries",
        );
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
                "was not downloaded",
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
        std::fs::write(dir.join("other"), b"protected").unwrap();
        symlink(
            dir.join("other"),
            dir.join("denmark-20260411-with-indexdata.osm.pbf"),
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
            tools: &tools,
            today: CivilDate::parse_yyyymmdd("20260411").unwrap(),
        };
        assert!(run(&req).unwrap_err().contains("registered by 'other'"));
        assert_eq!(tools.fetches.get(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::read(dir.join("other")).unwrap(), b"protected");
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
        let lock = DatasetLock::acquire(&dir, "denmark").unwrap();
        lock.set_identity("download.geofabrik.de/europe/denmark")
            .unwrap();
        drop(lock);
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
            tools: &tools,
            today: CivilDate::today(),
        };
        let before = std::fs::read_to_string(&config_path).unwrap();
        let lock = DatasetLock::acquire(&dir, "denmark").unwrap();
        lock.set_identity("wrong source").unwrap();
        drop(lock);
        assert!(run(&req).is_err());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), before);
        let lock = DatasetLock::acquire(&dir, "denmark").unwrap();
        lock.set_identity("download.geofabrik.de/europe/denmark")
            .unwrap();
        drop(lock);
        // A later transfer failure must leave both adoption and learned seq
        // unregistered, even though seq 13 was already published.
        assert!(
            run(&Request {
                osc_seq: Some(14),
                ..req
            })
            .is_err()
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
        assert!(
            run(&Request {
                refresh: false,
                force: false,
                config: &config,
                ..req
            })
            .is_err()
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
            tools: &tools,
            today: CivilDate::from_unix_timestamp(0).unwrap(),
        };
        for key in ["", "base", "../bad", "a.b", "non ascii"] {
            assert!(
                run(&Request {
                    snapshot: None,
                    as_snapshot: Some(key),
                    ..req
                })
                .is_err()
            );
        }
        assert!(
            run(&Request {
                region: "malta",
                ..req
            })
            .is_err()
        );
        assert_eq!(tools.fetches.get(), 0);
        assert!(run(&req).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(tools.fetches.get(), 1);
        // Recovery must adopt raw but regenerate any unregistered indexed file.
        std::fs::write(
            dir.join("denmark-20260411-with-indexdata.osm.pbf"),
            b"stale",
        )
        .unwrap();
        let lock = DatasetLock::acquire(&dir, "denmark").unwrap();
        lock.set_identity("wrong").unwrap();
        drop(lock);
        tools.fail_cat.set(false);
        assert!(run(&req).is_err());
        let lock = DatasetLock::acquire(&dir, "denmark").unwrap();
        lock.set_identity("download.geofabrik.de/europe/denmark")
            .unwrap();
        drop(lock);
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
        assert!(
            run(&Request {
                config: &updated,
                ..req
            })
            .is_err()
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
        // A shared registered diff needs no recovery identity, even if it
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
