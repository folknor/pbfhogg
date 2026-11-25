//! A generated tool protocol driving `verify getparents` end to end.
//!
//! The input's objects ride in its marker as OPL, as the getid fixture's
//! do. The graph gives every witness the check reads from osmium's outputs a
//! real object: way 7 references node 5 (a way parent of a requested node),
//! relation 1 has node 5, way 7 and relation 2 as members (a relation
//! parent of a requested node and way), relation 2 has relation 3 (a
//! relation parent of a requested relation), and relation 3, way 50 and
//! node 60 are requested objects no request's plain run selects, so only
//! add-self adds them. A clean run must therefore carry no coverage-gap
//! claim at all.
//!
//! Both fakes answer getparents by their own selection from the operand's
//! OPL (a way naming a requested node, a relation with a requested member,
//! the requested objects themselves under add-self), in the operand's
//! record order; the fake osmium also answers fileinfo, `cat -f opl` (with
//! `-t relation` too) and `sort`, all from the operand's records. Every
//! fault is on pbfhogg's side, so each osmium answer is a function of its
//! operand's bytes and the reference cache stays live.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};

const ROOT: &str = "getparents-fixture";
const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";
const EXIT_NEEDLE: &str = "fixture getparents refused";

const META: &str = "v1 dV c1 t i1 u";
/// The fixture's objects in canonical order.
const OBJECTS: [&str; 8] = [
    "n5 T x1 y1",
    "n6 Tname=six x2 y2",
    "n60 T x3 y3",
    "w7 T Nn5,n6",
    "w50 T Nn60",
    "r1 T Mn5@,w7@,r2@",
    "r2 T Mr3@",
    "r3 T M",
];
/// The order the unsorted input holds them in.
const SHUFFLED: [usize; 8] = [5, 3, 0, 7, 2, 4, 6, 1];

fn opl(order: impl Iterator<Item = usize>) -> String {
    let mut out = String::new();
    for i in order {
        let (head, rest) = OBJECTS[i].split_once(' ').unwrap();
        out.push_str(&format!("{head} {META} {rest}\n"));
    }
    out
}

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    let sorted = dir.join("input.pbf");
    fake_pbf::write(&sorted, &[SORTED], &opl(0..OBJECTS.len()));
    let unsorted = dir.join("unsorted.pbf");
    fake_pbf::write(&unsorted, &[], &opl(SHUFFLED.into_iter()));
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, ROOT.into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    // (fault, unsorted input, the failed comparison, the error's needle)
    let cases: [(&str, bool, Option<&str>, Option<&str>); 8] = [
        ("", false, None, None),
        ("", true, None, None),
        ("drop-parent", false, Some("getparents N diff"), None),
        (
            "drop-self",
            false,
            Some("getparents R --add-self diff"),
            None,
        ),
        (
            "sort-feature",
            false,
            Some("getparents M sort feature"),
            None,
        ),
        (
            "locations",
            false,
            Some("pbfhogg getparents W output declares LocationsOnWays as its command documents"),
            None,
        ),
        (
            "claims-sorted",
            true,
            Some("pbfhogg getparents W output claims Sort.Type_then_ID on unsorted input"),
            None,
        ),
        ("exit", false, None, Some(EXIT_NEEDLE)),
    ];
    for (i, (fault, is_unsorted, failed, error)) in cases.into_iter().enumerate() {
        let input = if is_unsorted { &unsorted } else { &sorted };
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        std::fs::write(dir.join("calls"), "").unwrap();
        let name = format!(
            "getparents-{i}-{}",
            if fault.is_empty() { "clean" } else { fault }
        );
        let mut claim = None;
        let judged = super::judge(&mut h, &name, |h| {
            let result = checks::getparents::run(h, input, true);
            if let Ok(c) = &result {
                claim.clone_from(c);
            }
            result
        });
        match (failed, error) {
            (None, None) => {
                assert_eq!(
                    judged.status,
                    harness::CheckStatus::Passed,
                    "{name}: {judged:?}"
                );
                assert!(judged.failed.is_empty(), "{name}: {judged:?}");
                assert_eq!(judged.error, None, "{name}: {judged:?}");
                clean_proof(&dir, &name, input, is_unsorted, claim.as_deref());
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

/// What proves every arm of a clean run ran: no coverage-gap claim at
/// all, and on unsorted input the sorted-copy claim alone and osmium sorting
/// both the input and pbfhogg's outputs (the first unsorted run misses the
/// cache).
fn clean_proof(dir: &Path, name: &str, input: &Path, unsorted: bool, claim: Option<&str>) {
    let expected = unsorted.then_some(crate::verify_input_order::CLAIM_UNSORTED);
    assert_eq!(claim, expected, "{name}");
    if unsorted {
        let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
        let shown = input.to_str().unwrap();
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with(&format!("sort {shown} -o "))),
            "{name}: {calls}"
        );
        assert!(
            calls
                .lines()
                .any(|l| l.starts_with("sort ") && l.contains("pbfhogg-getparents")),
            "{name}: {calls}"
        );
    }
}

fn kind_index(letter: u8) -> usize {
    match letter {
        b'n' => 0,
        b'w' => 1,
        b'r' => 2,
        _ => panic!("unknown kind letter {}", letter as char),
    }
}

/// An OPL record's `(kind index, id)`.
fn key(line: &str) -> (usize, i64) {
    let head = line.split(' ').next().unwrap();
    (kind_index(head.as_bytes()[0]), head[1..].parse().unwrap())
}

/// Every object a record references: a way's nodes, a relation's members.
fn references(line: &str) -> Vec<(usize, i64)> {
    let mut out = Vec::new();
    let way = line.starts_with('w');
    for field in line.split(' ').skip(1) {
        let items = match field.as_bytes().first() {
            Some(b'N') if way => &field[1..],
            Some(b'M') if !way => &field[1..],
            _ => continue,
        };
        for item in items.split(',').filter(|i| !i.is_empty()) {
            let end = item.find(['x', '@']).unwrap_or(item.len());
            out.push((
                kind_index(item.as_bytes()[0]),
                item[1..end].parse().unwrap(),
            ));
        }
    }
    out
}

/// The records getparents selects for `wanted`, in operand order.
fn getparents(objects: &str, wanted: &[(usize, i64)], add_self: bool) -> Vec<String> {
    objects
        .lines()
        .filter(|l| {
            (add_self && wanted.contains(&key(l)))
                || references(l).iter().any(|r| wanted.contains(r))
        })
        .map(str::to_owned)
        .collect()
}

fn joined(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// osmium fileinfo's report of `objects`, computed from its records.
fn fileinfo(objects: &str) -> String {
    let mut counts = [0_u64; 3];
    let mut min = [0_i64; 3];
    let mut max = [0_i64; 3];
    let mut ordered = true;
    let mut repeated = false;
    let mut last: Option<(usize, i64)> = None;
    for line in objects.lines() {
        let (kind, id) = key(line);
        if let Some(previous) = last {
            ordered &= previous <= (kind, id);
            repeated |= previous == (kind, id);
        }
        last = Some((kind, id));
        if counts[kind] == 0 {
            min[kind] = id;
            max[kind] = id;
        }
        min[kind] = min[kind].min(id);
        max[kind] = max[kind].max(id);
        counts[kind] += 1;
    }
    let kinds = |v: [String; 3]| {
        format!(
            r#"{{"nodes":{},"ways":{},"relations":{}}}"#,
            v[0], v[1], v[2]
        )
    };
    let versions = if ordered {
        format!(r#","multiple_versions":{repeated}"#)
    } else {
        String::new()
    };
    format!(
        r#"{{"data":{{"objects_ordered":{ordered}{versions},"count":{},"minid":{},"maxid":{}}}}}"#,
        kinds(counts.map(|c| c.to_string())),
        kinds(min.map(|c| c.to_string())),
        kinds(max.map(|c| c.to_string())),
    )
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let input = Path::new(
        *args[1..]
            .iter()
            .find(|a| Path::new(a).is_absolute())
            .unwrap(),
    );
    let root = input
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ROOT))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let calls = root.join("calls");
    let mut log = std::fs::read_to_string(&calls).unwrap_or_default();
    log.push_str(&args.join(" "));
    log.push('\n');
    std::fs::write(&calls, log).unwrap();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    let objects = fake_pbf::full_marker(input);
    let declares_sorted = fake_pbf::features(input).iter().any(|f| f == SORTED);
    match args[0] {
        "fileinfo" => {
            assert_eq!(args[1..4], ["-e", "-j", "--no-crc"], "{args:?}");
            print!("{}", fileinfo(&objects));
        }
        "cat" => {
            assert!(fake_pbf::is_opl_cat(&args), "{args:?}");
            let text = if args.contains(&"-t") {
                assert_eq!(value("-t"), "relation", "{args:?}");
                assert_eq!(value("-f"), "opl", "{args:?}");
                objects
                    .lines()
                    .filter(|l| l.starts_with('r'))
                    .map(|l| format!("{l}\n"))
                    .collect()
            } else {
                fake_pbf::project(&args, &objects)
            };
            std::fs::write(value("-o"), text).unwrap();
        }
        "sort" => {
            assert!(!args.contains(&"-f"), "{args:?}");
            let mut lines: Vec<&str> = objects.lines().collect();
            lines.sort_by_key(|l| key(l));
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            fake_pbf::write(Path::new(value("-o")), &[SORTED], &text);
        }
        "getparents" => {
            let wanted: Vec<(usize, i64)> = args
                .iter()
                .filter(|a| {
                    a.len() > 1
                        && a.starts_with(['n', 'w', 'r'])
                        && a[1..].bytes().all(|b| b.is_ascii_digit())
                })
                .map(|a| (kind_index(a.as_bytes()[0]), a[1..].parse().unwrap()))
                .collect();
            assert!(!wanted.is_empty(), "{args:?}");
            let ours = args.contains(&"--output");
            let add_self = args.contains(&if ours { "--add-self" } else { "-s" });
            if ours {
                assert!(args.contains(&"--direct-io"), "{args:?}");
                assert!(args.contains(&"--staging-dir"), "{args:?}");
                assert!(!args.contains(&"-s"), "{args:?}");
            } else {
                assert!(args.contains(&"--overwrite"), "{args:?}");
                assert!(!args.contains(&"--add-self"), "{args:?}");
            }
            let only = |kind| wanted.iter().all(|&(k, _)| k == kind);
            let mixed = !only(0) && !only(1) && !only(2);
            let mut lines = getparents(&objects, &wanted, add_self);
            let mut features: Vec<&str> = if declares_sorted {
                vec![SORTED]
            } else {
                vec![]
            };
            if ours {
                match fault.as_str() {
                    "exit" => {
                        eprintln!("{EXIT_NEEDLE}");
                        return 1;
                    }
                    "drop-parent" if only(0) && !add_self => lines.retain(|l| key(l) != (2, 1)),
                    "drop-self" if only(2) && add_self => lines.retain(|l| key(l) != (2, 3)),
                    "sort-feature" if mixed && !add_self => features.clear(),
                    "locations" if only(1) && !add_self => features.push(LOW),
                    "claims-sorted" if only(1) && !add_self => features.push(SORTED),
                    _ => {}
                }
            }
            let output = value(if ours { "--output" } else { "-o" });
            fake_pbf::write(Path::new(output), &features, &joined(&lines));
        }
        _ => panic!("unexpected getparents fixture invocation: {args:?}"),
    }
    0
}
