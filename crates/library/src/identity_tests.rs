#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn plan(text: &str) -> Document {
    Document::of("plan", &json::parse("plan", text.as_bytes()).unwrap()).unwrap()
}

#[test]
fn a_digest_is_over_the_canonical_bytes_whatever_the_spelling() {
    let a = plan(r#"{"calls": {"a": {"n": 1, "m": [true, null]}}}"#);
    let b = plan(r#"{"calls":{"a":{"m":[true,null],"n":1}}}"#);
    assert_eq!(a, b);
    assert_eq!(a.canonical, r#"{"calls":{"a":{"m":[true,null],"n":1}}}"#);
    assert_eq!(
        a.digest,
        format!(
            "plan-v1:{:032x}",
            xxhash_rust::xxh3::xxh3_128(a.canonical.as_bytes())
        )
    );
    assert_ne!(a, plan(r#"{"calls":{"a":{"m":[null,true],"n":1}}}"#));
}

#[test]
fn a_generated_tree_is_identified_by_layout_and_bytes_without_framing_collisions() {
    use std::os::unix::ffi::OsStrExt as _;
    let owner = pbfhogg_test_scratch::scratch();
    let root = owner.path();
    let make = |name: &str, link_target: &[u8], file: &[u8]| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(link_target), dir.join("a"))
            .unwrap();
        std::fs::write(dir.join(std::ffi::OsStr::from_bytes(file)), b"same").unwrap();
        dir
    };
    // A link target holding a newline beside a file whose name holds one:
    // a newline-framed encoding of the two trees would be byte-equal.
    let a = make("a", b"x\nb", b"c");
    let b = make("b", b"x", b"b\nc");
    let ia = generated(&a, RoleKind::Directory).unwrap();
    let ib = generated(&b, RoleKind::Directory).unwrap();
    assert!(ia.starts_with("tree-v1:"));
    assert_ne!(ia, ib);

    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        generated(&empty, RoleKind::Directory)
            .unwrap()
            .starts_with("tree-v1:")
    );
    assert_eq!(
        generated(&root.join("missing"), RoleKind::File).unwrap(),
        "absent"
    );
    let file = root.join("f.pbf");
    std::fs::write(&file, b"pbf").unwrap();
    assert_eq!(
        generated(&file, RoleKind::Pbf).unwrap(),
        format!("xxh128:{}", pbfhogg_devkit::file_digest(&file).unwrap())
    );
    assert!(generated(&file, RoleKind::Directory).is_err());
    assert!(
        generated(&a.join("a"), RoleKind::File).is_err(),
        "a link is never followed"
    );
}
