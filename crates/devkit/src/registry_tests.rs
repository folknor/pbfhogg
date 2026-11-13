#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::{Path, PathBuf};

use crate::{Config, FileEntry};

fn root_with(files: &[(&str, &str)]) -> (pbfhogg_test_scratch::TestScratch, PathBuf) {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    for (name, text) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    (scratch, root)
}

fn entry(file: &str, xxhash: &str) -> FileEntry {
    FileEntry {
        file: file.into(),
        xxhash: Some(xxhash.into()),
        seq: None,
    }
}

const HASH: &str = "0123456789abcdef0123456789abcdef";

fn read(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}

#[test]
fn rotation_moves_primary_tables_under_the_snapshot() {
    let before = "\
project = \"pbfhogg\"

[h.datasets.planet]
origin = \"planet.openstreetmap.org\"
download_date = \"2026-02-23\"

[h.datasets.planet.pbf.raw]
file = \"planet-20260223.osm.pbf\"
seq = 4912

[h.datasets.planet.osc.4913]
file = \"planet-seq4913.osc.gz\"
xxhash = \"0123456789ABCDEF0123456789abcdef\"
";
    let (_s, root) = root_with(&[("brokkr.toml", before)]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "planet").unwrap();
    edit.rotate_primary_to_snapshot("20260223", "2026-02-23", "2026-04-11")
        .unwrap();
    edit.commit().unwrap();
    let after = read(&root, "brokkr.toml");
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let view = config.dataset("h", "planet").unwrap().unwrap();
    assert!(view.primary.pbf.is_empty(), "{after}");
    assert!(view.primary.osc.is_empty(), "{after}");
    assert_eq!(view.primary.download_date.as_deref(), Some("2026-04-11"));
    let snap = &view.snapshots["20260223"];
    assert_eq!(snap.download_date.as_deref(), Some("2026-02-23"));
    assert_eq!(snap.pbf["raw"].seq, Some(4912));
    assert!(snap.osc.contains_key(&4913));
    let header = after.find("[h.datasets.planet.snapshot.20260223]").unwrap();
    let sub = after
        .find("[h.datasets.planet.snapshot.20260223.pbf.raw]")
        .unwrap();
    assert!(header < sub, "{after}");
}

#[test]
fn edits_preserve_comments_and_escape_names() {
    let before =
        "# top\nproject = \"pbfhogg\"\n\n# kept\n[h.datasets.d]\norigin = \"x\" # trailing\n";
    let (_s, root) = root_with(&[("brokkr.toml", before)]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "d").unwrap();
    edit.set_pbf("raw", &entry("odd \"name\\.osm.pbf", HASH))
        .unwrap();
    edit.set_osc(4705, &entry("d-seq4705.osc.gz", HASH))
        .unwrap();
    edit.commit().unwrap();
    let after = read(&root, "brokkr.toml");
    assert!(after.starts_with(before), "{after}");
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let view = config.dataset("h", "d").unwrap().unwrap();
    assert_eq!(view.primary.pbf["raw"].file, "odd \"name\\.osm.pbf");
    assert_eq!(view.primary.osc[&4705].xxhash.as_deref(), Some(HASH));
}

#[test]
fn uncommitted_edits_write_nothing() {
    let before = "project = \"pbfhogg\"\n";
    let (_s, root) = root_with(&[("brokkr.toml", before)]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "d").unwrap();
    edit.set_pbf("raw", &entry("r.osm.pbf", HASH)).unwrap();
    drop(edit);
    assert_eq!(read(&root, "brokkr.toml"), before);
}

#[test]
fn the_included_file_holding_the_dataset_is_edited() {
    let root_text = "project = \"pbfhogg\"\ninclude = [\"hosts/h.toml\"]\n";
    let (_s, root) = root_with(&[
        ("brokkr.toml", root_text),
        ("hosts/h.toml", "[h.datasets.d]\norigin = \"x\"\n"),
    ]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "d").unwrap();
    edit.set_pbf("raw", &entry("r.osm.pbf", HASH)).unwrap();
    edit.commit().unwrap();
    assert_eq!(read(&root, "brokkr.toml"), root_text);
    assert!(read(&root, "hosts/h.toml").contains("[h.datasets.d.pbf.raw]"));
}

#[test]
fn a_new_dataset_goes_to_the_file_holding_the_host_datasets() {
    let root_text = "project = \"pbfhogg\"\ninclude = [\"hosts/h.toml\"]\n";
    let (_s, root) = root_with(&[
        ("brokkr.toml", root_text),
        ("hosts/h.toml", "[h.datasets.other]\norigin = \"x\"\n"),
    ]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "new").unwrap();
    edit.set_origin("planet.openstreetmap.org").unwrap();
    edit.commit().unwrap();
    assert_eq!(read(&root, "brokkr.toml"), root_text);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(
        config
            .dataset("h", "new")
            .unwrap()
            .unwrap()
            .origin
            .as_deref(),
        Some("planet.openstreetmap.org")
    );
}

#[test]
fn a_split_dataset_and_a_doubly_spelled_host_are_refused() {
    let (_s, root) = root_with(&[
        (
            "brokkr.toml",
            "project = \"p\"\ninclude = [\"a.toml\"]\n[h.datasets.d.pbf.raw]\nfile = \"r\"\n",
        ),
        ("a.toml", "[h.datasets.d]\norigin = \"x\"\n"),
    ]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert!(config.edit_dataset("h", "d").is_err());

    let (_s, root) = root_with(&[(
        "brokkr.toml",
        "project = \"p\"\n[h]\ndata = \"x\"\n[host.h]\ndata = \"y\"\n",
    )]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert!(config.edit_dataset("h", "d").is_err());
}

#[test]
fn both_host_namespaces_are_kept() {
    for (text, explicit) in [
        (
            "project = \"p\"\n[host.h.datasets.d]\norigin = \"x\"\n",
            true,
        ),
        ("project = \"p\"\n[h.datasets.d]\norigin = \"x\"\n", false),
    ] {
        let (_s, root) = root_with(&[("brokkr.toml", text)]);
        let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
        let mut edit = config.edit_dataset("h", "d").unwrap();
        edit.set_pbf("raw", &entry("r.osm.pbf", HASH)).unwrap();
        edit.commit().unwrap();
        let parsed: toml::Table = toml::from_str(&read(&root, "brokkr.toml")).unwrap();
        assert_eq!(parsed.contains_key("host"), explicit);
        assert_eq!(parsed.contains_key("h"), !explicit);
    }
}

#[test]
fn sequence_insert_preserves_entry_and_refuses_replacement() {
    let (_s, root) = root_with(&[(
        "brokkr.toml",
        "[h.datasets.d.pbf.raw]\nfile = \"raw\" # keep\n",
    )]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "d").unwrap();
    assert!(edit.set_pbf_seq("absent", 10).is_err());
    assert!(edit.set_pbf_seq("raw", u64::MAX).is_err());
    edit.set_pbf_seq("raw", 10).unwrap();
    assert!(edit.set_pbf_seq("raw", 11).is_err());
    edit.commit().unwrap();
    assert!(read(&root, "brokkr.toml").contains("# keep"));
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(
        config.dataset("h", "d").unwrap().unwrap().primary.pbf["raw"].seq,
        Some(10)
    );
}

#[test]
fn host_inventory_includes_composed_datasets_and_all_generations() {
    let (_scratch, root) = root_with(&[
        (
            "brokkr.toml",
            "include = [\"host.toml\"]\n[host.h.datasets.a.pbf.raw]\nfile = \"a\"\n[host.empty]\n",
        ),
        (
            "host.toml",
            "[host.h.datasets.b.snapshot.s.osc.12]\nfile = \"b\"\n",
        ),
    ]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let views = config.dataset_views("h").unwrap();
    assert_eq!(
        views.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(views["a"].primary.pbf["raw"].file, "a");
    assert_eq!(views["b"].snapshots["s"].osc[&12].file, "b");
    assert!(config.dataset_views("empty").unwrap().is_empty());
    assert!(config.dataset_views("absent").is_err());
}

#[test]
fn snapshot_sequence_insert_never_replaces_or_creates_a_pbf() {
    let (_scratch, root) = root_with(&[(
        "brokkr.toml",
        "[h.datasets.d.snapshot.s.pbf.raw]\nfile = \"raw\" # keep\n",
    )]);
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let mut edit = config.edit_dataset("h", "d").unwrap();
    assert!(edit.set_snapshot_pbf_seq("s", "absent", 10).is_err());
    assert!(edit.set_snapshot_pbf_seq("s", "raw", u64::MAX).is_err());
    edit.set_snapshot_pbf_seq("s", "raw", 10).unwrap();
    assert!(edit.set_snapshot_pbf_seq("s", "raw", 11).is_err());
    edit.commit().unwrap();
    assert!(read(&root, "brokkr.toml").contains("# keep"));
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(
        config.dataset("h", "d").unwrap().unwrap().snapshots["s"].pbf["raw"].seq,
        Some(10)
    );
}
