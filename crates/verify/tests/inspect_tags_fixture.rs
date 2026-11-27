//! A generated tool protocol driving `verify inspect-tags` end to end.
//!
//! The input is a header-only fixture whose marker names [`MARKER`]. The
//! pbfhogg fake counts [`TAGS`] and prints JSON string literals. The osmium
//! fake writes XML from the same tags, so the verifier must recover the
//! counts, ordering and key bytes itself. The keys hold a double quote,
//! a backslash, a tab and a newline. The two
//! scopes report differently (a node key and a node occurrence of a way
//! key), so a check that dropped either way filter fails a verdict, and
//! every invocation is logged so the driver sees both scopes run.
//!
//! Every fault is on pbfhogg's side, so osmium's cached answer is the same
//! across cases and the cached projection stays live.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};

const ROOT: &str = "inspect-tags-fixture";
const MARKER: &str = "inspect-tags-v1";

/// `(is a way, key)` for every tag of the fixture's objects.
const TAGS: &[(bool, &[u8])] = &[
    (false, b"amenity"),
    (false, b"q\"uote"),
    (false, b"highway"),
    (true, b"highway"),
    (true, b"tab\tkey"),
    (true, b"nl\nkey"),
    (true, b"highway"),
    (true, b"back\\slash"),
];

const VERDICT_ALL: &str = "inspect tags (all kinds) vs osmium tags-count";
const VERDICT_WAYS: &str = "inspect tags (ways) vs osmium tags-count";
const EXIT_NEEDLE: &str = "fixture inspect tags refused";

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    fake_pbf::write(&input, &[], MARKER);
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, ROOT.into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    let shown = input.to_str().unwrap();
    // (fault, direct I/O, the failed comparison, the error's needle)
    let cases: [(&str, bool, Option<&str>, Option<&str>); 7] = [
        ("", true, None, None),
        ("", false, None, None),
        ("count-all", true, Some(VERDICT_ALL), None),
        ("order-way", true, Some(VERDICT_WAYS), None),
        (
            "malformed",
            true,
            None,
            Some("pbfhogg tags report at byte 12: expected a count and a tab"),
        ),
        ("nul", true, None, Some("with an embedded NUL")),
        ("exit", true, None, Some(EXIT_NEEDLE)),
    ];
    for (i, (fault, direct_io, failed, error)) in cases.into_iter().enumerate() {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        std::fs::write(dir.join("calls"), "").unwrap();
        let name = format!(
            "inspect-tags-{i}-{}",
            if fault.is_empty() { "clean" } else { fault }
        );
        let judged = super::judge(&mut h, &name, |h| {
            checks::inspect_tags::run(h, &input, direct_io)
        });
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        match (failed, error) {
            (None, None) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Passed,
                    "{name}: {judged:?}"
                );
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
                assert_eq!(judged.error, None, "{name}: {judged:?}");
                let dio = if direct_io { " --direct-io" } else { "" };
                let mut expected =
                    format!("inspect tags {shown}{dio}\ninspect tags {shown} --type way{dio}\n");
                if i == 0 {
                    // One cold decoding supplies both scopes.
                    expected = format!(
                        "inspect tags {shown}{dio}\ncat {shown} -f osm,add_metadata=false\n\
                         inspect tags {shown} --type way{dio}\n"
                    );
                }
                assert_eq!(calls, expected, "{name}");
            }
            (Some(failed), None) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Failed,
                    "{name}: {judged:?}"
                );
                assert_eq!(judged.failed, [failed], "{name}: {judged:?}");
                assert_eq!(judged.error, None, "{name}: {judged:?}");
            }
            (_, Some(error)) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Failed,
                    "{name}: {judged:?}"
                );
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
                let got = judged.error.as_deref().unwrap_or_default();
                assert!(got.contains(error), "{name}: {judged:?}");
            }
        }
    }
    std::fs::write(dir.join("fault"), "").unwrap();
}

/// The fixture's rows for one scope, ordered as both tools print them.
fn rows(ways: bool) -> Vec<(usize, Vec<u8>)> {
    let mut counts: Vec<(usize, Vec<u8>)> = Vec::new();
    for (way, key) in TAGS {
        if ways && !way {
            continue;
        }
        match counts.iter_mut().find(|(_, k)| k == key) {
            Some((n, _)) => *n += 1,
            None => counts.push((1, key.to_vec())),
        }
    }
    counts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    counts
}

/// pbfhogg's quoting, written out by hand rather than through a JSON
/// library so the fake shares no code with the verifier's decoder.
fn json_key(key: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    for &b in key {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0 => out.extend_from_slice(b"\\u0000"),
            _ => out.push(b),
        }
    }
    out.push(b'"');
    out
}

fn report(rows: &[(usize, Vec<u8>)], quote: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for (count, key) in rows {
        out.extend_from_slice(format!("{count}\t").as_bytes());
        out.extend(quote(key));
        out.push(b'\n');
    }
    out
}

fn xml() -> Vec<u8> {
    let mut out = b"<?xml version='1.0' encoding='UTF-8'?>\n<osm version=\"0.6\">\n".to_vec();
    for (id, &(way, key)) in TAGS.iter().enumerate() {
        let kind = if way { "way" } else { "node" };
        out.extend_from_slice(format!("  <{kind} id=\"{id}\">\n    <tag k=\"").as_bytes());
        for &b in key {
            match b {
                b'"' => out.extend_from_slice(b"&quot;"),
                b'\t' => out.extend_from_slice(b"&#x9;"),
                b'\n' => out.extend_from_slice(b"&#xA;"),
                _ => out.push(b),
            }
        }
        out.extend_from_slice(format!("\" v=\"\"/>\n  </{kind}>\n").as_bytes());
    }
    out.extend_from_slice(b"</osm>\n");
    out
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let input = Path::new(args.iter().find(|a| a.ends_with(".pbf")).unwrap());
    let root = input
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ROOT))
        .unwrap();
    assert_eq!(fake_pbf::marker(input), MARKER, "{args:?}");
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let calls = root.join("calls");
    let mut log = std::fs::read_to_string(&calls).unwrap_or_default();
    if let [
        "cat",
        _,
        "-f",
        "osm,add_metadata=false",
        "-o",
        output,
        "--overwrite",
    ] = args[..]
    {
        log.push_str(&args[..4].join(" "));
        log.push('\n');
        std::fs::write(&calls, log).unwrap();
        std::fs::write(output, xml()).unwrap();
        return 0;
    }
    log.push_str(&args.join(" "));
    log.push('\n');
    std::fs::write(&calls, log).unwrap();
    let mut out = std::io::stdout();
    let text = match args[..] {
        ["inspect", "tags", _, ref rest @ ..] => {
            assert!(!args.contains(&"--staging-dir"), "{args:?}");
            let ways = match rest {
                [] | ["--direct-io"] => false,
                ["--type", "way"] | ["--type", "way", "--direct-io"] => true,
                _ => panic!("unexpected inspect tags argv: {args:?}"),
            };
            let mut rows = rows(ways);
            match fault.as_str() {
                "count-all" if !ways => rows[0].0 += 1,
                "order-way" if ways => rows.swap(1, 2),
                "nul" => rows[1].1.push(0),
                _ => {}
            }
            let mut text = report(&rows, json_key);
            if fault == "malformed" {
                // A valid first row, then bytes no row starts with.
                text = report(&rows[..1], json_key);
                text.extend_from_slice(b"garbage\n");
            }
            if fault == "exit" {
                std::io::Write::write_all(&mut out, &text).unwrap();
                eprintln!("{EXIT_NEEDLE}");
                return 1;
            }
            text
        }
        _ => panic!("unexpected inspect-tags fixture invocation: {args:?}"),
    };
    std::io::Write::write_all(&mut out, &text).unwrap();
    0
}
