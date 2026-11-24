//! Import a completed producer output. Immutable, digest-qualified names
//! keep replacement crash-safe: the old registry always names old bytes.
//! Neither the caller's file nor displaced snapshot files are deleted.
use std::path::Path;

use pbfhogg_devkit::{Config, FileEntry, digest_open, file_digest, open_regular};

use crate::{date::CivilDate, layout, lock::DatasetLock, output};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Producer {
    Cat,
    Repack,
    Degrade,
}

pub(crate) struct Request<'a> {
    pub(crate) config: &'a Config,
    pub(crate) host: &'a str,
    pub(crate) dataset: &'a str,
    pub(crate) file: &'a Path,
    pub(crate) snapshot: &'a str,
    pub(crate) replace: bool,
    pub(crate) producer: Producer,
    pub(crate) strip_indexdata: bool,
}

fn key(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn run(req: &Request<'_>) -> Result<(), String> {
    if !key(req.dataset) || !key(req.snapshot) || req.snapshot == "base" {
        return Err(
            "dataset and snapshot keys must match [A-Za-z0-9_-]+; snapshot 'base' is reserved"
                .into(),
        );
    }
    if req.strip_indexdata && req.producer != Producer::Degrade {
        return Err("--strip-indexdata is only valid with --producer degrade".into());
    }
    let dir = req.config.data_dir(req.host).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _lock = DatasetLock::acquire(req.config, req.host, &dir, req.dataset, &[req.file])?;
    let view = req
        .config
        .dataset(req.host, req.dataset)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("dataset '{}' is not registered", req.dataset))?;
    if view.snapshots.contains_key(req.snapshot) && !req.replace {
        return Err(format!(
            "snapshot '{}' is already registered; use --replace-snapshot",
            req.snapshot
        ));
    }
    // Resolve the owner and prepare the entire transaction before publishing.
    let mut edit = req
        .config
        .edit_dataset(req.host, req.dataset)
        .map_err(|e| e.to_string())?;
    if req.replace {
        edit.remove_snapshot(req.snapshot)
            .map_err(|e| e.to_string())?;
    }
    let variant = if req.strip_indexdata {
        "raw"
    } else {
        "indexed"
    };
    // One descriptor serves the digest, the copy and the re-check, so a file
    // swapped in at the name afterwards (a FIFO would hang a by-name copy
    // under brokkr's machine lock) is never opened. Nonemptiness is counted
    // in the hashed bytes, not stat'ed beside them.
    let mut input = open_regular(req.file).map_err(|e| e.to_string())?;
    let (digest, bytes) = digest_open(&mut input, req.file).map_err(|e| e.to_string())?;
    if bytes == 0 {
        return Err("promotion requires a nonempty regular output file".into());
    }
    let name = format!(
        "{}-{}-{variant}-{digest}.osm.pbf",
        req.dataset, req.snapshot
    );
    let dest = dir.join(&name);
    publish_output(req, &mut input, &dir, &dest, &digest)?;
    let date = CivilDate::parse_yyyymmdd(req.snapshot).unwrap_or_else(CivilDate::today);
    edit.set_snapshot_header(req.snapshot, &date.iso())
        .map_err(|e| e.to_string())?;
    edit.set_snapshot_pbf(
        req.snapshot,
        variant,
        &FileEntry {
            file: name,
            xxhash: Some(digest),
            seq: None,
        },
    )
    .map_err(|e| e.to_string())?;
    for line in edit.commit().map_err(|e| e.to_string())? {
        output::note(&line);
    }
    Ok(())
}

fn publish_output(
    req: &Request<'_>,
    input: &mut std::fs::File,
    dir: &Path,
    dest: &Path,
    digest: &str,
) -> Result<(), String> {
    // An existing final file is reusable only if its bytes match. Never
    // follow or overwrite a pre-existing symlink, including dangling ones.
    match std::fs::symlink_metadata(dest) {
        Ok(meta) => {
            if !meta.is_file() || file_digest(dest).map_err(|e| e.to_string())? != digest {
                return Err(format!(
                    "{} already exists with different bytes or type",
                    dest.display()
                ));
            }
            // A retry reuses what an earlier run may not have made durable.
            layout::make_durable(dest)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Recovery must not fill an absent destination any registration
            // resolves through, dangling aliases included.
            layout::claim(req.config, req.host, dir, dest)?;
            // Even an unusual manually configured staging name belongs to
            // the registry, and the output must survive its staging.
            let staged = layout::stage(req.config, req.host, dir, req.dataset, dest, &[req.file])?;
            copy_staged(input, &staged).map_err(|e| format!("copy output: {e}"))?;
            // Hash the staged copy and the input descriptor again after
            // copying: concurrent input changes must never turn the
            // registered pin into a claim about other bytes.
            if file_digest(&staged).map_err(|e| e.to_string())? != digest
                || digest_open(input, req.file).map_err(|e| e.to_string())?.0 != digest
            {
                return Err("output changed during promotion".into());
            }
            layout::publish(&staged, dest)?;
        }
        Err(e) => return Err(format!("{}: {e}", dest.display())),
    }
    Ok(())
}

/// Copy the hashed input descriptor into the freshly cleared staging name.
/// `create_new` refuses anything planted there since, a symlink included;
/// the input's permissions carry over as `std::fs::copy` carried them.
fn copy_staged(input: &mut std::fs::File, staged: &Path) -> std::io::Result<()> {
    use std::io::Seek as _;
    input.rewind()?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staged)?;
    std::io::copy(input, &mut out)?;
    out.set_permissions(input.metadata()?.permissions())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn promotion_replaces_whole_snapshot_without_destroying_shared_files() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let path = root.join("brokkr.toml");
        std::fs::write(
            &path,
            "[h]\ndata = 'data'\n[h.datasets.tiny.pbf.raw]\nfile = 'base'\n",
        )
        .unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"locally generated producer output").unwrap();
        let load = || Config::load(&path, None).unwrap();
        let config = load();
        let mut req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &input,
            snapshot: "20261113",
            replace: false,
            producer: Producer::Cat,
            strip_indexdata: false,
        };
        run(&req).unwrap();
        let old_config = std::fs::read(&path).unwrap();
        let config = load();
        req.config = &config;
        assert!(run(&req).unwrap_err().contains("already registered"));
        assert_eq!(old_config, std::fs::read(&path).unwrap());
        let before = config.dataset("h", "tiny").unwrap().unwrap();
        let old = &before.snapshots["20261113"].pbf["indexed"];
        assert_eq!(
            before.snapshots["20261113"].download_date.as_deref(),
            Some("2026-11-13")
        );
        assert_eq!(old.xxhash.as_ref().unwrap(), &file_digest(&input).unwrap());
        let mut edit = config.edit_dataset("h", "tiny").unwrap();
        edit.set_snapshot_osc("20261113", 4, old).unwrap();
        edit.commit().unwrap();
        let config = load();
        req.config = &config;
        req.replace = true;
        req.producer = Producer::Repack;
        run(&req).unwrap();
        req.producer = Producer::Degrade;
        req.strip_indexdata = true;
        run(&req).unwrap();
        let after = load().dataset("h", "tiny").unwrap().unwrap();
        let snap = &after.snapshots["20261113"];
        assert!(snap.osc.is_empty());
        assert_eq!(
            snap.pbf.keys().map(String::as_str).collect::<Vec<_>>(),
            ["raw"]
        );
        assert!(root.join("data").join(&old.file).exists());
        assert_eq!(
            std::fs::read(&input).unwrap(),
            b"locally generated producer output"
        );
        assert_eq!(before.primary, after.primary);
    }

    #[test]
    fn failed_registry_commit_keeps_old_bytes_and_retry_reuses_publication() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let path = root.join("brokkr.toml");
        std::fs::write(&path, "[h]\ndata = 'data'\n[h.datasets.tiny]\n").unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"first output").unwrap();
        let config = Config::load(&path, None).unwrap();
        let mut req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &input,
            snapshot: "named",
            replace: false,
            producer: Producer::Repack,
            strip_indexdata: false,
        };
        run(&req).unwrap();
        let config = Config::load(&path, None).unwrap();
        req.config = &config;
        req.replace = true;
        let old =
            config.dataset("h", "tiny").unwrap().unwrap().snapshots["named"].pbf["indexed"].clone();
        let before = std::fs::read(&path).unwrap();
        std::fs::write(&input, b"second output").unwrap();
        // Force the writer's staging open to fail after PBF publication.
        let blocked = root.join(".brokkr.toml.tmp");
        std::fs::create_dir(&blocked).unwrap();
        assert!(run(&req).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            file_digest(&root.join("data").join(&old.file)).unwrap(),
            old.xxhash.unwrap()
        );
        let published = root.join("data").join(format!(
            "tiny-named-indexed-{}.osm.pbf",
            file_digest(&input).unwrap()
        ));
        assert!(published.exists());
        std::fs::remove_dir(blocked).unwrap();
        run(&req).unwrap();
        let after = Config::load(&path, None)
            .unwrap()
            .dataset("h", "tiny")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.snapshots["named"].pbf["indexed"]
                .xxhash
                .as_ref()
                .unwrap(),
            &file_digest(&input).unwrap()
        );
    }

    #[test]
    fn staging_recovery_protects_input_and_registered_aliases() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let dir = root.join("data");
        std::fs::create_dir(&dir).unwrap();
        let path = root.join("brokkr.toml");
        std::fs::write(&path, "[h]\ndata = 'data'\n[h.datasets.tiny]\n").unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"generated").unwrap();
        let digest = file_digest(&input).unwrap();
        let dest = dir.join(format!("tiny-named-indexed-{digest}.osm.pbf"));
        let staged = layout::partial("tiny", &dest);
        std::fs::rename(&input, &staged).unwrap();
        let config = Config::load(&path, None).unwrap();
        let mut req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &staged,
            snapshot: "named",
            replace: false,
            producer: Producer::Degrade,
            strip_indexdata: false,
        };
        assert!(run(&req).unwrap_err().contains("staging name"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"generated");
        std::fs::rename(&staged, &input).unwrap();
        req.file = &input;
        // A separate dataset references staging through a symlink alias.
        std::os::unix::fs::symlink(&staged, dir.join("alias")).unwrap();
        let mut edit = config.edit_dataset("h", "other").unwrap();
        edit.set_pbf(
            "raw",
            &FileEntry {
                file: "alias".into(),
                xxhash: None,
                seq: None,
            },
        )
        .unwrap();
        edit.commit().unwrap();
        let config = Config::load(&path, None).unwrap();
        req.config = &config;
        // Resolving the alias looks up the staging entry, so the staging
        // name is protected whether or not a file sits there yet.
        assert!(run(&req).unwrap_err().contains("already registered"));
        std::fs::write(&staged, b"registered bytes").unwrap();
        assert!(run(&req).unwrap_err().contains("already registered"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"registered bytes");
        assert!(!dest.exists());
    }

    #[test]
    fn polygons_and_dangling_aliases_of_the_final_name_are_protected() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let dir = root.join("data");
        std::fs::create_dir(&dir).unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"generated").unwrap();
        let digest = file_digest(&input).unwrap();
        let dest = dir.join(format!("tiny-named-indexed-{digest}.osm.pbf"));
        let staged = layout::partial("tiny", &dest);
        let path = root.join("brokkr.toml");
        // A polygon registered at the staging name is never unlinked.
        std::fs::write(&staged, b"polygon").unwrap();
        let text = format!(
            "[h]\ndata = 'data'\n[h.datasets.tiny]\n[h.datasets.other.polygon.p]\nfile = {:?}\n",
            staged.file_name().unwrap().to_str().unwrap()
        );
        std::fs::write(&path, &text).unwrap();
        let config = Config::load(&path, None).unwrap();
        let req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &input,
            snapshot: "named",
            replace: false,
            producer: Producer::Cat,
            strip_indexdata: false,
        };
        assert!(run(&req).unwrap_err().contains("staging name"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"polygon");
        std::fs::remove_file(&staged).unwrap();
        // A registration through a dangling alias of the absent final name
        // is never filled.
        std::os::unix::fs::symlink(&dest, dir.join("alias")).unwrap();
        let text =
            "[h]\ndata = 'data'\n[h.datasets.tiny]\n[h.datasets.other.pmtiles.t]\nfile = 'alias'\n";
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path, None).unwrap();
        let err = run(&Request {
            config: &config,
            ..req
        })
        .unwrap_err();
        assert!(err.contains("publication name"), "{err}");
        assert!(dest.symlink_metadata().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    /// The lock name is never filled for a registration or the promoted
    /// input, and a symlink, FIFO or hard link at it is not a lock.
    #[test]
    fn the_lock_name_refuses_what_it_does_not_own() {
        use std::os::unix::fs::symlink;
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let dir = root.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let path = root.join("brokkr.toml");
        let lock = dir.join(".tiny.download.lock");
        let elsewhere = root.join("elsewhere");
        let base = "[h]\ndata = 'data'\n[h.datasets.tiny]\norigin = 'local'\n";
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Config::load(&path, None).unwrap()
        };

        // A registration naming the lock, directly or through a dangling
        // symlink, or a promotion input that does: nothing is created.
        let direct = load(&format!(
            "{base}[h.datasets.other.pmtiles.t]\nfile = '.tiny.download.lock'\n"
        ));
        let err = DatasetLock::acquire(&direct, "h", &dir, "tiny", &[])
            .err()
            .unwrap();
        assert!(err.contains("dataset lock name"), "{err}");
        symlink(&lock, dir.join("alias")).unwrap();
        let aliased = load(&format!(
            "{base}[h.datasets.other.pmtiles.t]\nfile = 'alias'\n"
        ));
        assert!(DatasetLock::acquire(&aliased, "h", &dir, "tiny", &[]).is_err());
        let plain = load(base);
        assert!(DatasetLock::acquire(&plain, "h", &dir, "tiny", &[&dir.join("alias")]).is_err());
        assert!(lock.symlink_metadata().is_err());

        // A symlink at the lock name, live or dangling, is not followed.
        symlink(&elsewhere, &lock).unwrap();
        let err = DatasetLock::acquire(&plain, "h", &dir, "tiny", &[])
            .err()
            .unwrap();
        assert!(err.contains("symlink"), "{err}");
        assert!(elsewhere.symlink_metadata().is_err());
        std::fs::write(&elsewhere, b"x").unwrap();
        assert!(DatasetLock::acquire(&plain, "h", &dir, "tiny", &[]).is_err());
        std::fs::remove_file(&lock).unwrap();

        // A hard link shares another file's inode.
        std::fs::hard_link(&elsewhere, &lock).unwrap();
        let err = DatasetLock::acquire(&plain, "h", &dir, "tiny", &[])
            .err()
            .unwrap();
        assert!(err.contains("single link"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        // A FIFO with no writer refuses at once.
        let c_path = std::ffi::CString::new(lock.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a scalar mode.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let err = DatasetLock::acquire(&plain, "h", &dir, "tiny", &[])
            .err()
            .unwrap();
        assert!(err.contains("regular file"), "{err}");
        std::fs::remove_file(&lock).unwrap();

        // An ordinary lock is created, then reused in place.
        drop(DatasetLock::acquire(&plain, "h", &dir, "tiny", &[]).unwrap());
        assert!(DatasetLock::acquire(&plain, "h", &dir, "tiny", &[]).is_ok());
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"x");
    }

    #[test]
    fn refusals_preserve_registry_and_input() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let path = root.join("brokkr.toml");
        let text = "[h]\ndata = 'data'\n[h.datasets.tiny]\norigin = 'local'\n";
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path, None).unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"generated").unwrap();
        let mut req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &input,
            snapshot: "base",
            replace: false,
            producer: Producer::Cat,
            strip_indexdata: false,
        };
        for snap in ["base", "../escape", "", "bad.key"] {
            req.snapshot = snap;
            assert!(run(&req).is_err());
        }
        req.snapshot = "ok";
        req.strip_indexdata = true;
        assert!(run(&req).is_err());
        req.strip_indexdata = false;
        req.dataset = "missing";
        assert!(run(&req).is_err());
        req.dataset = "tiny";
        let dir = root.join("data");
        let lock = DatasetLock::acquire(&config, "h", &dir, "tiny", &[]).unwrap();
        assert!(run(&req).unwrap_err().contains("held"));
        drop(lock);
        let digest = file_digest(&input).unwrap();
        let dest = dir.join(format!("tiny-ok-indexed-{digest}.osm.pbf"));
        std::os::unix::fs::symlink(&input, &dest).unwrap();
        assert!(run(&req).is_err());
        std::fs::remove_file(&dest).unwrap();
        std::fs::write(&dest, b"wrong bytes").unwrap();
        assert!(run(&req).is_err());
        std::fs::remove_file(&dest).unwrap();
        std::fs::write(&input, b"").unwrap();
        assert!(run(&req).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[test]
    fn a_fifo_swapped_in_after_hashing_refuses_without_opening_it() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let path = root.join("brokkr.toml");
        std::fs::write(
            &path,
            "[h]\ndata = 'data'\n[h.datasets.tiny]\norigin = 'local'\n",
        )
        .unwrap();
        let config = Config::load(&path, None).unwrap();
        let dir = root.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let input = root.join("output.osm.pbf");
        std::fs::write(&input, b"generated").unwrap();
        let req = Request {
            config: &config,
            host: "h",
            dataset: "tiny",
            file: &input,
            snapshot: "ok",
            replace: false,
            producer: Producer::Cat,
            strip_indexdata: false,
        };
        let mut held = open_regular(&input).unwrap();
        let (digest, _) = digest_open(&mut held, &input).unwrap();
        // Replace the name with a FIFO that has no writer: a by-name open
        // of it would block forever.
        let fifo = root.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path and a scalar mode.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        std::fs::rename(&fifo, &input).unwrap();
        let dest = dir.join(format!("tiny-ok-indexed-{digest}.osm.pbf"));
        let err = publish_output(&req, &mut held, &dir, &dest, &digest).unwrap_err();
        assert!(err.contains("changed while it was hashed"), "{err}");
        assert!(!dest.exists());
    }
}
