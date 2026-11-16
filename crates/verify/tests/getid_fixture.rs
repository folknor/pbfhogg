//! A generated tool protocol driving `verify getid-removeid` end to end.
//!
//! The input's objects ride in its marker as OPL (node 1 and way 1, the way
//! carrying an inline coordinate), as the refs fixture's do; its data blobs
//! are one indexed node blob and one indexed way blob, so the verifier's
//! indexdata walk reads both kinds and requests `n1 w1`. The fakes select
//! records by ID from the OPL, and the fake osmium decodes it by the
//! requested OPL options ([`fake_pbf::project`]).
//!
//! Every pbfhogg output but one drops `LocationsOnWays` (it re-encodes or
//! requests a way); `getid --invert n1` copies the way blob whole and keeps
//! it, and osmium removeid is asked to keep it too. One inline coordinate
//! altered on pbfhogg's outputs alone must therefore fail exactly that
//! comparison, by name, which witnesses the both-declared branch of
//! `Policy::between` there; a clean run passes.
use std::ffi::OsString;
use std::path::Path;

use crate::{checks, fake_pbf, harness, process, reference, reference_cache, run};

const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";
const ROOT: &str = "getid-fixture";
/// The CLI's rendering of the library's `header.way_metadata_loss`.
const LOSS_WARNING: &str = "Warning: the input declares LocationsOnWays, which the output does \
                            not declare: this command does not keep it on every way it writes, \
                            or cannot vouch for it in the output";

const OBJECTS: &str = "n1 v1 dV c1 t i1 u T x1 y1\nw1 v1 dV c1 t i1 u T Nn1x1y1\n";

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    fake_pbf::write_blobs(
        &input,
        &[SORTED, LOW],
        OBJECTS,
        &[
            fake_pbf::Data::of(0, &[1], true),
            fake_pbf::Data::of(1, &[1], true),
        ],
    );
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, ROOT.into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    for fault in ["", "coords"] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        let name = format!("getid-{}", if fault.is_empty() { "clean" } else { fault });
        let judged = super::judge(&mut h, &name, |h| {
            checks::getid_removeid::run(h, &input, true)
        });
        assert_eq!(judged.error, None, "{name}: {judged:?}");
        if fault.is_empty() {
            assert_eq!(judged.status, harness::CheckStatus::Passed, "{judged:?}");
            assert!(judged.failed.is_empty(), "{judged:?}");
        } else {
            assert_eq!(judged.status, harness::CheckStatus::Failed, "{judged:?}");
            assert_eq!(
                judged.failed,
                ["getid --invert without way IDs diff"],
                "{judged:?}"
            );
        }
    }
    std::fs::write(dir.join("fault"), "").unwrap();
}

/// The `(kind letter, ID)` of an OPL record.
fn key(line: &str) -> String {
    line.split(' ').next().unwrap().to_owned()
}

/// The records of `objects` with the node references of the ways among
/// them added (`-r`, `--add-referenced`): the nodes those ways name.
fn with_referenced(objects: &str, keys: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = keys.to_vec();
    for line in objects.lines().filter(|l| keys.contains(&key(l))) {
        if let Some(refs) = line.split(' ').find_map(|f| f.strip_prefix('N')) {
            for item in refs.split(',').filter(|i| !i.is_empty()) {
                wanted.push(item.split('x').next().unwrap().to_owned());
            }
        }
    }
    wanted
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let input = Path::new(
        *args
            .iter()
            .find(|a| Path::new(a).is_absolute() && a.ends_with(".pbf"))
            .unwrap(),
    );
    let root = input
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == ROOT))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    let objects = fake_pbf::full_marker(input);
    if fake_pbf::is_opl_cat(&args) {
        std::fs::write(value("-o"), fake_pbf::project(&args, &objects)).unwrap();
        return 0;
    }
    let declares = fake_pbf::features(input).iter().any(|f| f == LOW);
    let keys: Vec<String> = args
        .iter()
        .filter(|a| {
            a.len() > 1
                && a.starts_with(['n', 'w', 'r'])
                && a[1..].bytes().all(|b| b.is_ascii_digit())
        })
        .map(|a| (*a).to_owned())
        .collect();
    let ours = args.contains(&"--output");
    if ours {
        assert!(args.contains(&"--direct-io"));
        assert!(args.contains(&"--staging-dir"));
    }
    let (wanted, invert, keeps) = match args[0] {
        "getid" if args.contains(&"--invert") => {
            assert!(ours);
            let way_requested = keys.iter().any(|k| k.starts_with('w'));
            (keys.clone(), true, !way_requested)
        }
        "getid" if args.contains(&"--add-referenced") || args.contains(&"-r") => {
            assert_eq!(args.contains(&"-v"), !ours);
            (with_referenced(&objects, &keys), false, false)
        }
        "getid" => (keys.clone(), false, false),
        "removeid" => {
            assert!(!ours);
            let asked = args.contains(&"pbf,locations_on_ways=true");
            assert_eq!(asked, declares, "{args:?}");
            (keys.clone(), true, asked)
        }
        _ => panic!("unexpected getid fixture invocation: {args:?}"),
    };
    let mut selected = String::new();
    for line in objects.lines() {
        if wanted.contains(&key(line)) != invert {
            let mut line = line.to_owned();
            if ours && fault == "coords" && line.starts_with('w') {
                assert_eq!(line.matches("Nn1x1y1").count(), 1, "{line}");
                line = line.replace("Nn1x1y1", "Nn1x2y1");
            }
            selected.push_str(&line);
            selected.push('\n');
        }
    }
    let kept = declares && keeps;
    let features: &[&str] = if kept { &[SORTED, LOW] } else { &[SORTED] };
    fake_pbf::write(
        Path::new(value(if ours { "--output" } else { "-o" })),
        features,
        &selected,
    );
    if ours && declares && !kept {
        eprintln!("{LOSS_WARNING}");
    }
    0
}
