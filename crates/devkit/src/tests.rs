#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::path::Path;

use crate::{Config, file_digest, user_config_path};

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn include_chain_namespaces_snapshot_and_digest() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    write(root, "data/base.pbf", "base bytes");
    write(root, "data/snapshot.pbf", "snapshot bytes");
    let digest = file_digest(&root.join("data/base.pbf")).unwrap();
    write(
        root,
        "shared/nested/host.toml",
        &format!(
            "[host.machine.datasets.region.pbf.indexed]\nfile = 'base.pbf'\nxxhash = '{digest}'\n"
        ),
    );
    write(
        root,
        "shared/host.toml",
        "include = ['nested/host.toml']\n[host.machine.datasets.region.snapshot.old.pbf.raw]\nfile = 'snapshot.pbf'\n",
    );
    write(
        root,
        "brokkr.toml",
        "include = ['shared/host.toml']\n[host.machine]\ndata = 'data'\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let input = config
        .resolve_input("machine", "region", None, "source", "indexed")
        .unwrap();
    assert_eq!(input.role, "source");
    assert_eq!(input.variant, "indexed");
    assert_eq!(input.path, root.join("data/base.pbf"));
    assert_eq!(input.expected_digest, Some(digest.clone()));
    assert_eq!(input.observed_digest, digest);
    let snap = config
        .resolve_input("machine", "region", Some("old"), "other", "raw")
        .unwrap();
    assert_eq!(snap.expected_digest, None);
    assert_eq!(
        snap.observed_digest,
        file_digest(&root.join("data/snapshot.pbf")).unwrap()
    );
    assert!(
        config
            .resolve_input("machine", "region", Some("old"), "source", "indexed")
            .is_err()
    );
    assert!(
        config
            .resolve_input("machine", "region", Some("missing"), "source", "indexed")
            .is_err()
    );
    assert!(
        config
            .resolve_input("machine", "region", Some("../bad"), "source", "indexed")
            .is_err()
    );
    // Same semantics with the legacy namespace, including base alias.
    let text = std::fs::read_to_string(root.join("shared/nested/host.toml"))
        .unwrap()
        .replace("host.machine", "machine");
    write(root, "legacy.toml", &text);
    let legacy = Config::load(&root.join("legacy.toml"), None).unwrap();
    assert_eq!(
        legacy
            .resolve_input("machine", "region", Some("base"), "source", "indexed")
            .unwrap(),
        input
    );
}

#[test]
fn pins_are_checked_and_unpinned_bytes_are_observed_afresh() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    write(root, "data/input", "abc");
    // Published XXH3-128 vector, not a second invocation of our hash helper.
    assert_eq!(
        file_digest(&root.join("data/input")).unwrap(),
        "06b05ab6733a618578af5f94892f3950"
    );
    for (pin, succeeds) in [
        ("", true),
        ("xxhash = '06B05AB6733A618578AF5F94892F3950'", true),
        ("xxhash = '00000000000000000000000000000000'", false),
        ("xxhash = 'bogus'", false),
        ("xxhash = 42", false),
        ("sha256 = 'retired'", false),
    ] {
        write(
            root,
            "brokkr.toml",
            &format!("[m.datasets.d.pbf.raw]\nfile = 'input'\n{pin}\n"),
        );
        let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
        assert_eq!(
            config.resolve_input("m", "d", None, "r", "raw").is_ok(),
            succeeds,
            "{pin}"
        );
    }
    write(
        root,
        "brokkr.toml",
        "[m.datasets.d.pbf.raw]\nfile = 'input'\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    let before = config.resolve_input("m", "d", None, "r", "raw").unwrap();
    write(root, "data/input", "changed");
    assert_ne!(
        before.observed_digest,
        config
            .resolve_input("m", "d", None, "r", "raw")
            .unwrap()
            .observed_digest
    );
    std::fs::remove_file(root.join("data/input")).unwrap();
    assert!(config.resolve_input("m", "d", None, "r", "raw").is_err());
    std::fs::create_dir(root.join("data/input")).unwrap();
    assert!(config.resolve_input("m", "d", None, "r", "raw").is_err());
}

#[test]
fn precedence_user_layer_and_named_arrays() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    write(
        root,
        "user.toml",
        "[[script_check]]\nname = 'same'\ncommand = 'user'\n[[script_check]]\nname = 'retained'\ncommand = 'kept'\n",
    );
    write(
        root,
        "a.toml",
        "[m]\ndata = 'a'\n[cargo_env]\nKEY = { command = 'old', path = 'old' }\n",
    );
    write(root, "b.toml", "[m]\ndata = 'b'\n");
    write(
        root,
        "brokkr.toml",
        "include = ['a.toml', 'b.toml']\n[m]\ndata = 'root'\n[[script_check]]\nname = 'same'\ncommand = 'project'\n[cargo_env]\nKEY = { command = 'new' }\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), Some(&root.join("user.toml"))).unwrap();
    let table = config.composed();
    assert_eq!(table["m"]["data"].as_str(), Some("root"));
    assert_eq!(table["script_check"].as_array().unwrap().len(), 2);
    assert_eq!(
        table["script_check"][1]["command"].as_str(),
        Some("project")
    );
    assert!(table["cargo_env"]["KEY"].get("path").is_none());
    write(root, "user.toml", "[m]\ndata = 'forbidden'\n");
    assert!(Config::load(&root.join("brokkr.toml"), Some(&root.join("user.toml"))).is_err());
    assert!(Config::load(&root.join("brokkr.toml"), Some(&root.join("absent"))).is_ok());
}

#[test]
fn invalid_includes_and_ambiguous_hosts_are_refused() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    for text in [
        "include = ['brokkr.toml']",
        "include = ['other.toml', './other.toml']",
        "include = ['absent.toml']",
        "include = 'other.toml'",
        "include = [42]",
        "include = ['']",
        "not valid toml",
        "results_server = 'private'",
    ] {
        write(root, "other.toml", "[m]\ndata = 'data'\n");
        write(root, "brokkr.toml", text);
        assert!(
            Config::load(&root.join("brokkr.toml"), None).is_err(),
            "{text}"
        );
    }
    write(root, "brokkr.toml", "include = ['other.toml']");
    write(root, "other.toml", "project = 'wrong'");
    assert!(Config::load(&root.join("brokkr.toml"), None).is_err());
    write(
        root,
        "brokkr.toml",
        "[m]\ndata = 'data'\n[host.m]\ndata = 'data'\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert!(
        config
            .resolve_input("m", "d", None, "r", "raw")
            .unwrap_err()
            .to_string()
            .contains("both")
    );
}

#[test]
fn user_path_selection_has_no_process_environment_dependency() {
    use std::ffi::OsStr;
    assert_eq!(
        user_config_path(Some(OsStr::new("")), None, Some(OsStr::new("/home/user"))),
        None
    );
    assert_eq!(
        user_config_path(Some(OsStr::new("chosen")), None, None).unwrap(),
        Path::new("chosen")
    );
    assert_eq!(
        user_config_path(None, Some(OsStr::new("/config")), None).unwrap(),
        Path::new("/config/brokkr/brokkr.toml")
    );
    assert_eq!(
        user_config_path(
            None,
            Some(OsStr::new("relative")),
            Some(OsStr::new("/home/user"))
        )
        .unwrap(),
        Path::new("/home/user/.config/brokkr/brokkr.toml")
    );
    assert_eq!(user_config_path(None, None, None), None);
}

#[test]
fn absolute_data_and_file_paths_and_symlinked_config() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    write(&root, "shared/data/input", "contents");
    write(&root, "data/input", "project bytes");
    write(&root, "shared/include.toml", "included = true");
    write(
        &root,
        "shared/brokkr.toml",
        "include = ['include.toml']\n[host.check.datasets.d.pbf.raw]\nfile = 'input'\n",
    );
    std::os::unix::fs::symlink(root.join("shared/brokkr.toml"), root.join("link.toml")).unwrap();
    let config = Config::load(&root.join("link.toml"), None).unwrap();
    assert_eq!(config.project_root(), root);
    assert_eq!(config.composed()["included"].as_bool(), Some(true));
    assert_eq!(
        config
            .resolve_input("check", "d", None, "r", "raw")
            .unwrap()
            .path,
        root.join("data/input")
    );
    write(
        &root,
        "absolute.toml",
        &format!(
            "[m]\ndata = '{}'\n[m.datasets.d.pbf.raw]\nfile = 'input'\n",
            root.join("shared/data").display()
        ),
    );
    let config = Config::load(&root.join("absolute.toml"), None).unwrap();
    assert!(config.resolve_input("m", "d", None, "r", "raw").is_ok());
    write(
        &root,
        "absolute.toml",
        &format!(
            "[m.datasets.d.pbf.raw]\nfile = '{}'\n",
            root.join("shared/data/input").display()
        ),
    );
    let config = Config::load(&root.join("absolute.toml"), None).unwrap();
    assert!(config.resolve_input("m", "d", None, "r", "raw").is_ok());
}

#[test]
fn scratch_selection_shares_dataset_host_rules() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    write(
        root,
        "brokkr.toml",
        "[host.explicit]\nscratch = 'custom'\n[legacy]\n[bad]\nscratch = 42\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(
        config.scratch_root("explicit").unwrap(),
        root.join("custom")
    );
    assert_eq!(
        config.scratch_root("legacy").unwrap(),
        root.join("data/scratch")
    );
    assert!(config.scratch_root("bad").is_err());
    assert!(config.scratch_root("unknown").is_err());
}

#[test]
fn pbfhogg_host_is_not_reserved_by_a_library_child() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    write(&root, "data/input", "local fixture");
    for namespace in ["pbfhogg", "host.pbfhogg"] {
        write(
            &root,
            "brokkr.toml",
            &format!(
                "[{namespace}]\ndata = 'data'\nscratch = 'work'\n[{namespace}.library]\n[{namespace}.datasets.d.pbf.raw]\nfile = 'input'\n"
            ),
        );
        let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
        assert_eq!(config.data_dir("pbfhogg").unwrap(), root.join("data"));
        assert_eq!(config.scratch_root("pbfhogg").unwrap(), root.join("work"));
        assert!(
            config
                .resolve_input("pbfhogg", "d", None, "r", "raw")
                .is_ok()
        );
        config.edit_dataset("pbfhogg", "d").unwrap();
    }
    write(&root, "brokkr.toml", "[pbfhogg.library]\n[host.pbfhogg]\n");
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert!(
        config
            .data_dir("pbfhogg")
            .unwrap_err()
            .to_string()
            .contains("both")
    );
    assert!(config.edit_dataset("pbfhogg", "d").is_err());
    write(
        &root,
        "brokkr.toml",
        "[project]\n[host.project]\ndata = 'explicit'\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(config.data_dir("project").unwrap(), root.join("explicit"));
}

#[test]
fn selection_primitives_distinguish_absence_from_invalid() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = scratch.path();
    write(root, "data/a.osc", "change a");
    write(root, "data/b.osc", "change b");
    write(root, "data/polygon.json", "{}");
    write(root, "data/base.pbf", "base");
    write(
        root,
        "brokkr.toml",
        "
[m.datasets.d]
bbox = '-1, 2,3,4'
[m.datasets.d.osc.10]
file = 'a.osc'
[m.datasets.d.osc.11]
file = 'b.osc'
[m.datasets.d.polygon.z]
file = 'polygon.json'
[m.datasets.d.polygon.a]
file = 'polygon.json'
[m.datasets.d.pbf.raw]
file = 'base.pbf'
[m.datasets.d.snapshot.empty]
[m.datasets.d.snapshot.one.osc.12]
file = 'b.osc'
[m.datasets.d.snapshot.bad.osc.10]
file = 'missing.osc'
",
    );
    let c = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert!(c.select_osc("m", "d", None, None).is_err());
    assert_eq!(
        c.select_osc("m", "d", None, Some("10"))
            .unwrap()
            .unwrap()
            .variant,
        "10"
    );
    assert!(c.select_osc("m", "d", None, Some("12")).is_err());
    assert!(
        c.select_osc("m", "d", Some("empty"), None)
            .unwrap()
            .is_none()
    );
    assert!(c.select_osc("m", "d", Some("empty"), Some("10")).is_err());
    assert!(c.select_osc("m", "d", Some("missing"), None).is_err());
    assert!(c.select_osc("m", "d", Some("bad"), None).is_err());
    assert_eq!(
        c.select_osc("m", "d", Some("one"), None)
            .unwrap()
            .unwrap()
            .variant,
        "12"
    );
    let range = c.select_osc_range("m", "d", None, 10, 11).unwrap();
    assert_eq!(
        range.iter().map(|r| r.variant.as_str()).collect::<Vec<_>>(),
        ["10", "11"]
    );
    assert!(c.select_osc_range("m", "d", None, 10, 12).is_err());
    assert!(c.select_osc_range("m", "d", None, 11, 10).is_err());
    assert!(c.select_pbf("m", "d", None, "raw").unwrap().is_some());
    assert!(
        c.select_pbf("m", "d", Some("empty"), "raw")
            .unwrap()
            .is_none()
    );
    assert!(c.select_pbf("m", "d", Some("unknown"), "raw").is_err());
    let bbox = c.select_bbox("m", "d").unwrap().unwrap();
    assert_eq!(bbox.coordinates(), [-1.0, 2.0, 3.0, 4.0]);
    assert_eq!(bbox.to_pbfhogg_arg(), "-b=-1, 2,3,4");
    let polygons = c.select_polygons("m", "d").unwrap().unwrap();
    assert_eq!(
        polygons
            .iter()
            .map(|r| r.variant.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert!(c.select_polygon("m", "d", "missing").unwrap().is_none());
    for bad in ["1,2,3", "1,2,3,NaN", "1,2,inf,4", "a,2,3,4"] {
        assert!(crate::Bbox::parse(bad).is_err());
    }
}

#[test]
fn selection_primitives_validate_configured_entries() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = scratch.path();
    write(root, "data/a.osc", "change a");
    // Pins and file validation apply uniformly to every selected file kind.
    for kind in ["osc.1", "polygon.a", "pbf.raw"] {
        for entry in [
            "file = 'missing'",
            "file = 'a.osc'\nxxhash = '00000000000000000000000000000000'",
            "file = 3",
            "file = ''",
            "file = '.'",
            "file = 'a.osc'\nsha256 = 'retired'",
        ] {
            write(
                root,
                "brokkr.toml",
                &format!("[m.datasets.d.{kind}]\n{entry}\n"),
            );
            let c = Config::load(&root.join("brokkr.toml"), None).unwrap();
            let fails = match kind {
                "osc.1" => c.select_osc("m", "d", None, None).is_err(),
                "polygon.a" => c.select_polygons("m", "d").is_err(),
                _ => c.select_pbf("m", "d", None, "raw").is_err(),
            };
            assert!(fails, "{kind}: {entry}");
        }
    }
    for setting in [
        "osc = 3",
        "pbf = 3",
        "polygon = 3",
        "bbox = 3",
        "bbox = '1,2,NaN,4'",
    ] {
        write(root, "brokkr.toml", &format!("[m.datasets.d]\n{setting}\n"));
        let c = Config::load(&root.join("brokkr.toml"), None).unwrap();
        assert!(
            c.select_osc("m", "d", None, None).is_err()
                || c.select_pbf("m", "d", None, "raw").is_err()
                || c.select_polygons("m", "d").is_err()
                || c.select_bbox("m", "d").is_err()
        );
    }
}
