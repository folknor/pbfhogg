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
    pbfhogg_test_scratch::refused(
        config.resolve_input("machine", "region", Some("old"), "source", "indexed"),
        "missing or invalid table 'indexed'",
    );
    pbfhogg_test_scratch::refused(
        config.resolve_input("machine", "region", Some("missing"), "source", "indexed"),
        "missing or invalid table 'missing'",
    );
    pbfhogg_test_scratch::refused(
        config.resolve_input("machine", "region", Some("../bad"), "source", "indexed"),
        "invalid snapshot key '../bad'",
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
    pbfhogg_test_scratch::refused(
        config.resolve_input("m", "d", None, "r", "raw"),
        "data/input: No such file or directory",
    );
    std::fs::create_dir(root.join("data/input")).unwrap();
    pbfhogg_test_scratch::refused(
        config.resolve_input("m", "d", None, "r", "raw"),
        "data/input: expected a regular file",
    );
}

/// A FIFO with no writer at a registered name, directly or through a
/// symlink, refuses at once instead of blocking in the open; a symlink to a
/// regular file is still followed.
#[test]
fn a_registered_fifo_refuses_without_blocking() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    write(root, "data/real", "abc");
    let fifo = root.join("data/fifo");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path and a scalar mode.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    std::os::unix::fs::symlink(&fifo, root.join("data/to-fifo")).unwrap();
    std::os::unix::fs::symlink(root.join("data/real"), root.join("data/to-real")).unwrap();
    for (file, succeeds) in [("fifo", false), ("to-fifo", false), ("to-real", true)] {
        write(
            root,
            "brokkr.toml",
            &format!("[m.datasets.d.pbf.raw]\nfile = '{file}'\n"),
        );
        let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
        let resolved = config.resolve_input("m", "d", None, "r", "raw");
        assert_eq!(resolved.is_ok(), succeeds, "{file}");
        if !succeeds {
            assert!(
                resolved
                    .unwrap_err()
                    .to_string()
                    .contains("expected a regular file"),
                "{file}"
            );
        }
    }
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
    pbfhogg_test_scratch::refused(
        Config::load(&root.join("brokkr.toml"), Some(&root.join("user.toml"))),
        "'m' is not allowed in the user-wide config",
    );
    assert!(Config::load(&root.join("brokkr.toml"), Some(&root.join("absent"))).is_ok());
}

#[test]
fn invalid_includes_and_ambiguous_hosts_are_refused() {
    let scratch = pbfhogg_test_scratch::scratch();
    let canonical = std::fs::canonicalize(scratch.path()).unwrap();
    let root = canonical.as_path();
    let entries = "brokkr.toml: 'include' entries must be non-empty path strings";
    for (text, needle) in [
        ("include = ['brokkr.toml']", "brokkr.toml: include cycle"),
        (
            "include = ['other.toml', './other.toml']",
            "other.toml: included more than once",
        ),
        (
            "include = ['absent.toml']",
            "absent.toml: No such file or directory",
        ),
        (
            "include = 'other.toml'",
            "brokkr.toml: 'include' must be an array of paths",
        ),
        ("include = [42]", entries),
        ("include = ['']", entries),
        ("not valid toml", "brokkr.toml: TOML parse error at line 1"),
        (
            "results_server = 'private'",
            "brokkr.toml: 'results_server' belongs to the user config",
        ),
    ] {
        write(root, "other.toml", "[m]\ndata = 'data'\n");
        write(root, "brokkr.toml", text);
        pbfhogg_test_scratch::refused(Config::load(&root.join("brokkr.toml"), None), needle);
    }
    write(root, "brokkr.toml", "include = ['other.toml']");
    write(root, "other.toml", "project = 'wrong'");
    pbfhogg_test_scratch::refused(
        Config::load(&root.join("brokkr.toml"), None),
        "'project' belongs in the project's own brokkr.toml",
    );
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
    pbfhogg_test_scratch::refused(config.scratch_root("bad"), "'scratch' must be a string");
    pbfhogg_test_scratch::refused(config.scratch_root("unknown"), "unknown host 'unknown'");
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
    pbfhogg_test_scratch::refused(
        config.edit_dataset("pbfhogg", "d"),
        "host 'pbfhogg' is configured both as [pbfhogg] and [host.pbfhogg]",
    );
    write(
        &root,
        "brokkr.toml",
        "[project]\n[host.project]\ndata = 'explicit'\n",
    );
    let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
    assert_eq!(config.data_dir("project").unwrap(), root.join("explicit"));
}

/// One dataset with every selectable kind, an empty snapshot, and snapshots
/// whose OSC table is missing a file or carries a non-canonical key.
fn selection_fixture(root: &std::path::Path) -> Config {
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
[m.datasets.d.snapshot.padded.osc.011]
file = 'b.osc'
",
    );
    Config::load(&root.join("brokkr.toml"), None).unwrap()
}

#[test]
fn selection_primitives_distinguish_absence_from_invalid() {
    let scratch = pbfhogg_test_scratch::scratch();
    let c = selection_fixture(scratch.path());
    pbfhogg_test_scratch::refused(
        c.select_osc("m", "d", None, None),
        "multiple OSC entries (10, 11); select --osc-seq",
    );
    assert_eq!(
        c.select_osc("m", "d", None, Some(10))
            .unwrap()
            .unwrap()
            .variant,
        "10"
    );
    assert!(
        c.select_osc("m", "d", Some("empty"), None)
            .unwrap()
            .is_none()
    );
    for (snapshot, seq, needle) in [
        ("empty", Some(10), "missing osc.10"),
        ("missing", None, "missing or invalid table 'missing'"),
        ("bad", None, "data/missing.osc: No such file or directory"),
    ] {
        pbfhogg_test_scratch::refused(c.select_osc("m", "d", Some(snapshot), seq), needle);
    }
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
    for (refusal, needle) in [
        (
            c.select_osc("m", "d", None, Some(12)).map(drop),
            "missing osc.12",
        ),
        (
            c.select_osc_range("m", "d", None, 10, 12).map(drop),
            "missing osc.12",
        ),
        (
            c.select_osc_range("m", "d", None, 11, 10).map(drop),
            "OSC range must be ascending",
        ),
    ] {
        pbfhogg_test_scratch::refused(refusal, needle);
    }
    // A non-canonical key refuses the table for every selection, so a sole
    // `011` is never selectable by one path and missing for another.
    let padded = Some("padded");
    for refusal in [
        c.select_osc("m", "d", padded, None).map(drop),
        c.select_osc("m", "d", padded, Some(11)).map(drop),
        c.select_osc_range("m", "d", padded, 11, 11).map(drop),
        c.has_osc("m", "d", padded).map(drop),
    ] {
        pbfhogg_test_scratch::refused(refusal, "osc key '011' is not a canonical sequence");
    }
}

#[test]
fn pbf_bbox_and_polygon_selection_distinguish_absence_from_invalid() {
    let scratch = pbfhogg_test_scratch::scratch();
    let c = selection_fixture(scratch.path());
    assert!(c.select_pbf("m", "d", None, "raw").unwrap().is_some());
    assert!(
        c.select_pbf("m", "d", Some("empty"), "raw")
            .unwrap()
            .is_none()
    );
    pbfhogg_test_scratch::refused(
        c.select_pbf("m", "d", Some("unknown"), "raw"),
        "missing or invalid table 'unknown'",
    );
    let bbox = c.select_bbox("m", "d").unwrap().unwrap();
    assert_eq!(bbox.coordinates(), [-1.0, 2.0, 3.0, 4.0]);
    assert_eq!(bbox.to_pbfhogg_arg(), "-b=-1, 2,3,4");
    assert_eq!(bbox.to_osmium_text().unwrap(), "-1,2,3,4");
    let polygons = c.select_polygons("m", "d").unwrap().unwrap();
    assert_eq!(
        polygons
            .iter()
            .map(|r| r.variant.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert!(c.select_polygon("m", "d", "missing").unwrap().is_none());
}

#[test]
fn bbox_parse_names_each_refusal() {
    for (bad, needle) in [
        ("1,2,3", "bbox requires four comma-separated coordinates"),
        ("1,2,3,NaN", "bbox component 3 must be finite"),
        ("1,2,inf,4", "bbox component 2 must be finite"),
        ("a,2,3,4", "invalid bbox component 0: 'a'"),
    ] {
        pbfhogg_test_scratch::refused(crate::Bbox::parse(bad), needle);
    }
}

/// Each edge reaches osmium as the decimal pbfhogg reads from the typed
/// text, in a spelling libosmium's parser takes as that same column: the
/// exponent applied exactly, the fraction truncated after eight digits.
#[test]
fn osmium_text_restates_each_edge_as_a_plain_decimal() {
    let one = |edge: &str| {
        crate::Bbox::parse(&format!("{edge},0,1,1"))
            .unwrap()
            .to_osmium_text()
            .map(|text| text.split(',').next().unwrap().to_owned())
    };
    for (edge, expected) in [
        ("12.4", "12.4"),
        ("13.0", "13.0"),
        ("-25.0", "-25.0"),
        ("180", "180"),
        ("0.000000001e2", "0.0000001"),
        ("5e-8", "0.00000005"),
        ("-5e-8", "-0.00000005"),
        ("5e-9", "0"),
        ("1.24e1", "12.4"),
        ("+12.", "12"),
        (" 2", "2"),
        (".5", "0.5"),
        ("00012.40", "12.40"),
        ("1.0000000499999999", "1.00000004"),
        ("0e1000000000000000000", "0"),
        ("1e-99999999999", "0"),
        ("1e9", "1000000000"),
    ] {
        assert_eq!(one(edge).as_deref(), Some(expected), "edge {edge:?}");
    }
    assert_eq!(one("1e10"), None);
    // A mantissa longer than any fixed exponent cap still cancels exactly.
    let long = format!("1{}e-100001", "0".repeat(100_001));
    assert_eq!(one(&long).as_deref(), Some("1.00000000"));
}

#[test]
fn selection_primitives_validate_configured_entries() {
    let scratch = pbfhogg_test_scratch::scratch();
    let root = scratch.path();
    write(root, "data/a.osc", "change a");
    // Pins and file validation apply uniformly to every selected file kind.
    for kind in ["osc.1", "polygon.a", "pbf.raw"] {
        for (entry, needle) in [
            (
                "file = 'missing'",
                "data/missing: No such file or directory",
            ),
            (
                "file = 'a.osc'\nxxhash = '00000000000000000000000000000000'",
                "data/a.osc: xxhash mismatch: expected 00000000000000000000000000000000",
            ),
            ("file = 3", "'file' must be a string"),
            ("file = ''", "file entry requires a non-empty 'file'"),
            ("file = '.'", "data/.: expected a regular file"),
            (
                "file = 'a.osc'\nsha256 = 'retired'",
                "dataset pin 'sha256' is retired; configure 'xxhash' instead",
            ),
        ] {
            write(
                root,
                "brokkr.toml",
                &format!("[m.datasets.d.{kind}]\n{entry}\n"),
            );
            let c = Config::load(&root.join("brokkr.toml"), None).unwrap();
            let refusal = match kind {
                "osc.1" => c.select_osc("m", "d", None, None).map(drop),
                "polygon.a" => c.select_polygons("m", "d").map(drop),
                _ => c.select_pbf("m", "d", None, "raw").map(drop),
            };
            pbfhogg_test_scratch::refused(refusal, needle);
        }
    }
    for (setting, needle) in [
        ("osc = 3", "'osc' must be a table"),
        ("pbf = 3", "'pbf' must be a table"),
        ("polygon = 3", "'polygon' must be a table"),
        ("bbox = 3", "'bbox' must be a string"),
        ("bbox = '1,2,NaN,4'", "bbox component 2 must be finite"),
    ] {
        write(root, "brokkr.toml", &format!("[m.datasets.d]\n{setting}\n"));
        let c = Config::load(&root.join("brokkr.toml"), None).unwrap();
        let refusal = match setting.split(' ').next() {
            Some("osc") => c.select_osc("m", "d", None, None).map(drop),
            Some("pbf") => c.select_pbf("m", "d", None, "raw").map(drop),
            Some("polygon") => c.select_polygons("m", "d").map(drop),
            _ => c.select_bbox("m", "d").map(drop),
        };
        pbfhogg_test_scratch::refused(refusal, needle);
    }
}
