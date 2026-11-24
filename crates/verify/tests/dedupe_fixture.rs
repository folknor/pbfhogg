//! A generated tool protocol driving `verify cat --dedupe` end to end.
//!
//! The input is four indexed node blobs, declaring `Sort.Type_then_ID` and
//! `LocationsOnWays`, whose marker carries a way with an inline coordinate:
//! the way exists in the fake decoding only, not in any blob. So no way blob
//! is ever in an overlap run, every merge keeps the declaration, and each of
//! the six merges compares inline coordinates (`Policy::between`, both
//! declared). This witnesses the check's wiring and the field policy it
//! passes; it does not witness a real way surviving dedupe, which no
//! self-merge can show (a real way blob is always in the self run).
//!
//! The fake `pbfhogg cat --dedupe` writes the layout the verifier's own
//! `dedupe_plan::predict` gives, so the layout arm passes by construction;
//! the plan is therefore also held to a hand-written trace per merge
//! ([`TRACES`]), which a wrong predictor fails independently, and a layout
//! fault (one blob after the last segment) and a summary fault must each
//! fail their arm in every merge by name.
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;

use crate::checks::dedupe_plan::{self, Segment};
use crate::{checks, fake_pbf, harness, pbf_frames, process, reference, reference_cache, run};

const SORTED: &str = "Sort.Type_then_ID";
const LOW: &str = "LocationsOnWays";
const ROOT: &str = "dedupe-fixture";

/// The input's node blobs.
const BLOBS: [&[i64]; 4] = [&[1, 2], &[3, 4], &[5, 6], &[7, 8]];

/// Each merge's plan, segment by segment: `P<input>:<blob>` a passthrough,
/// `R<min>-<reach>[<input>:<blob>,...]` a run and its members. The selection
/// is the input's even blobs, its complement the odd ones, and the window
/// `n4 n5`, bridging blobs 1 and 2.
const TRACES: [&str; 6] = [
    "self R1-2[0:0,1:0] R3-4[0:1,1:1] R5-6[0:2,1:2] R7-8[0:3,1:3]",
    "input+selected R1-2[0:0,1:0] P0:1 R5-6[0:2,1:1] P0:3",
    "selected+rest P0:0 P1:0 P0:1 P1:1",
    "rest+selected P1:0 P0:0 P1:1 P0:1",
    "input+window P0:0 R3-6[0:1,1:0,0:2] P0:3",
    "window+input P1:0 R3-6[1:1,0:0,1:2] P1:3",
];

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join(ROOT);
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    let blobs: Vec<_> = BLOBS
        .iter()
        .map(|ids| fake_pbf::Data::of(0, ids, true))
        .collect();
    fake_pbf::write_blobs(&input, &[SORTED, LOW], "sorted @w1", &blobs);
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, ROOT.into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    let labels = TRACES.map(|t| t.split(' ').next().unwrap());
    for (fault, arm) in [
        ("", None),
        ("coords", Some("vs input elements")),
        ("layout", Some("output blobs in the predicted layout")),
        ("summary", Some("summary blob counts as predicted")),
    ] {
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(&input).unwrap();
        std::fs::write(dir.join("fault"), fault).unwrap();
        for label in labels {
            let file = dir.join(format!("trace-{label}"));
            if file.exists() {
                std::fs::remove_file(file).unwrap();
            }
        }
        let name = format!("dedupe-{}", if fault.is_empty() { "clean" } else { fault });
        let judged = super::judge(&mut h, &name, |h| checks::cat_dedupe::run(h, &input, true));
        assert_eq!(judged.error, None, "{name}: {judged:?}");
        let trace: Vec<String> = labels
            .iter()
            .map(|l| std::fs::read_to_string(dir.join(format!("trace-{l}"))).unwrap())
            .collect();
        assert_eq!(trace, TRACES, "{name}");
        match arm {
            None => {
                assert_eq!(judged.status, harness::CheckStatus::Passed, "{judged:?}");
                assert!(judged.failed.is_empty(), "{judged:?}");
            }
            Some(arm) => {
                let want: Vec<String> = labels
                    .iter()
                    .map(|l| format!("pbfhogg cat --dedupe {l} {arm}"))
                    .collect();
                assert_eq!(judged.status, harness::CheckStatus::Failed, "{judged:?}");
                assert_eq!(judged.failed, want, "{name}");
            }
        }
    }
    std::fs::write(dir.join("fault"), "").unwrap();
}

/// The serialized `Blob` payload of `blob` in `path`.
fn payload(path: &Path, blob: &pbf_frames::ClaimedBlob) -> Vec<u8> {
    let bytes = std::fs::read(path).unwrap();
    let at = usize::try_from(blob.payload_offset).unwrap();
    bytes[at..at + usize::try_from(blob.datasize).unwrap()].to_vec()
}

/// A raw indexed node blob holding `ids` decoded from `path`'s `blob`.
fn node_ids(path: &Path, blob: &pbf_frames::ClaimedBlob) -> Vec<i64> {
    pbf_frames::blob_ids(
        path,
        pbf_frames::Origin::Other,
        blob.payload_offset,
        blob.datasize,
    )
    .unwrap()
    .into_iter()
    .map(|(kind, id)| {
        assert_eq!(kind, 0, "the dedupe fixture holds node blobs only");
        id
    })
    .collect()
}

/// Assert an osmium run writing PBF asks to keep inline locations exactly
/// when its input declares them, and answer whether it does.
fn asks_locations(args: &[&str]) -> bool {
    let locations = fake_pbf::features(Path::new(args[1]))
        .iter()
        .any(|f| f == LOW);
    assert_eq!(args.contains(&"pbf,locations_on_ways=true"), locations);
    locations
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    if fake_pbf::is_opl_cat(&args) {
        fake_pbf::opl_cat(&args);
        return 0;
    }
    let root = args
        .iter()
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .find_map(|p| {
            p.ancestors()
                .find(|a| a.file_name().is_some_and(|n| n == ROOT))
        })
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    let value = |flag| args[args.iter().position(|s| *s == flag).unwrap() + 1];
    match args[0] {
        "cat" => {
            assert_eq!(args[1], "--dedupe", "{args:?}");
            assert!(args.contains(&"--direct-io"));
            assert!(args.contains(&"--staging-dir"));
            dedupe(
                &fault,
                root,
                [Path::new(args[2]), Path::new(args[3])],
                Path::new(value("--output")),
            );
        }
        "tags-filter" => {
            // The selection is the input's even blobs, its complement the odd.
            let input = Path::new(args[1]);
            let locations = asks_locations(&args);
            let odd = usize::from(args.contains(&"--invert-match"));
            let entries = dedupe_plan::entries(input).unwrap();
            let blobs: Vec<_> = entries
                .iter()
                .skip(odd)
                .step_by(2)
                .map(|e| fake_pbf::Data {
                    claim: None,
                    payload: payload(input, &e.blob),
                })
                .collect();
            let features: &[&str] = if locations { &[SORTED, LOW] } else { &[SORTED] };
            fake_pbf::write_blobs(
                Path::new(value("-o")),
                features,
                &fake_pbf::full_marker(input),
                &blobs,
            );
        }
        "getid" => {
            let input = Path::new(args[1]);
            let locations = asks_locations(&args);
            let ids: Vec<i64> = args
                .iter()
                .filter_map(|a| a.strip_prefix('n').and_then(|id| id.parse().ok()))
                .collect();
            assert!(args.iter().all(|a| !a.starts_with(['w', 'r'])), "{args:?}");
            let features: &[&str] = if locations { &[SORTED, LOW] } else { &[SORTED] };
            fake_pbf::write_blobs(
                Path::new(value("-o")),
                features,
                &fake_pbf::full_marker(input),
                &[fake_pbf::Data::of(0, &ids, false)],
            );
        }
        _ => panic!("unexpected dedupe fixture invocation: {args:?}"),
    }
    0
}

/// The fake `pbfhogg cat --dedupe`: the predicted plan written out, each
/// passthrough's payload copied, each run's IDs merged into one blob, the
/// plan traced for the driver, and the summary printed; then `fault`.
fn dedupe(fault: &str, root: &Path, inputs: [&Path; 2], output: &Path) {
    let entries = inputs.map(|p| dedupe_plan::entries(p).unwrap());
    let plan = dedupe_plan::predict(&[&entries[0], &entries[1]]);
    let shape = dedupe_plan::shape(&plan);
    let label = output
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_prefix("pbfhogg-")
        .unwrap()
        .strip_suffix(".osm.pbf")
        .unwrap();
    let mut trace = label.to_owned();
    let mut blobs = Vec::new();
    for seg in &plan {
        match seg {
            Segment::Pass { input, entry } => {
                write!(trace, " P{input}:{}", entry.blob.index).unwrap();
                let ids = node_ids(inputs[*input], &entry.blob);
                blobs.push(fake_pbf::Data {
                    claim: Some((0, ids[0], ids[ids.len() - 1], ids.len() as u64)),
                    payload: payload(inputs[*input], &entry.blob),
                });
            }
            Segment::Run {
                min,
                reach,
                members,
                ..
            } => {
                let list: Vec<String> = members
                    .iter()
                    .map(|(i, e)| format!("{i}:{}", e.blob.index))
                    .collect();
                write!(trace, " R{min}-{reach}[{}]", list.join(",")).unwrap();
                let mut ids: Vec<i64> = members
                    .iter()
                    .flat_map(|(i, e)| node_ids(inputs[*i], &e.blob))
                    .collect();
                ids.sort_unstable();
                ids.dedup();
                blobs.push(fake_pbf::Data::of(0, &ids, true));
            }
        }
    }
    if fault == "layout" {
        blobs.push(fake_pbf::Data::of(0, &[100], true));
    }
    // One file per merge: merges run concurrently, so a shared log would be
    // a racing read-modify-write.
    std::fs::write(root.join(format!("trace-{label}")), trace).unwrap();

    let declared = inputs
        .iter()
        .all(|p| fake_pbf::features(p).iter().any(|f| f == LOW));
    // pbfhogg keeps the declaration when every input has it, re-encoding
    // rewritten ways with their coordinates.
    let features: &[&str] = if declared { &[SORTED, LOW] } else { &[SORTED] };
    let mut marker = fake_pbf::full_marker(inputs[0]);
    if fault == "coords" {
        marker.push_str(" @w2");
    }
    fake_pbf::write_blobs(output, features, &marker, &blobs);
    let total = shape.total + u64::from(fault == "summary");
    eprintln!(
        "Merged 0 elements: 0 nodes, 0 ways, 0 relations ({total} blobs: {} passthrough, {} \
         rewritten, 0 duplicates removed)",
        shape.passthrough, shape.rewritten
    );
}
