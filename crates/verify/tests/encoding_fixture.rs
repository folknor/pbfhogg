//! Generated encoding protocols, independent of installed tools and datasets.
use crate::{
    argv::PbfArgv, checks, harness, process, reference, reference_cache, run, verify_reencode,
};
use std::ffi::OsString;
use std::path::Path;

pub fn driver(life: process::Lifecycle, exe: &Path, dir: &Path) {
    let dir = dir.join("encoding-fixture");
    std::fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pbf");
    write_pbf(&input, false, 3, &["Sort.Type_then_ID"], &[]);
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "encoding".into(), true).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools { osmium: exe.into() },
        false,
        cache,
    );
    for fault in [
        "",
        "required-output",
        "content",
        "metadata",
        "out-coords",
        "index",
        "layout",
        "header",
        "refused",
        "published",
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        for (check, arms) in [
            (
                checks::repack::run as checks::ReadCheck,
                &[
                    ("repack default cap", 8000),
                    ("repack cap 3000", 3000),
                    ("repack cap 64000", 64000),
                ][..],
            ),
            (
                checks::degrade::run,
                &[("degrade --strip-locations", 8000)][..],
            ),
        ] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            let judged = super::judge(&mut h, "encoding", |h| check(h, &input, true));
            assert_encoding(fault, arms, &judged);
        }
    }
    // A LocationsOnWays input: the reference is osmium cat without
    // coordinates, and the coverage diff must find a way that lost them.
    // Requiring coordinates fails on an input that declares none, or whose
    // coverage diff finds none; without the requirement that is a gap.
    let low = dir.join("low.pbf");
    write_pbf(&low, false, 3, &["Sort.Type_then_ID"], &["LocationsOnWays"]);
    judge_degrade_pair(&mut h, exe, &dir, &input, &low);
    for (fault, path, required, passed) in [
        ("", &low, true, true),
        ("no-coords", &low, true, false),
        ("no-coords", &low, false, true),
        ("", &input, true, false),
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.run.watch(path).unwrap();
        let check = if required {
            checks::degrade::run_locations as checks::ReadCheck
        } else {
            checks::degrade::run
        };
        let judged = super::judge(&mut h, "degrade", |h| check(h, path, true));
        // A gap in the requirement is a refusal, never a failed comparison.
        assert!(judged.failed.is_empty(), "{judged:?}");
        assert_eq!(judged.error.is_none(), passed, "{judged:?}");
        assert_eq!(
            judged.status == harness::CheckStatus::Passed,
            passed,
            "fault={fault} required={required} input={}",
            path.display()
        );
    }
    // Only the exact refusal with no published output is accepted.
    for (fault, accepted) in [
        ("refused", true),
        ("published", false),
        ("wrong-advice", false),
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        h.run = run::Run::create(&dir, exe, true).unwrap();
        h.subdir("refusal").unwrap();
        let output = h.run.dir.join("refused.pbf");
        let result = verify_reencode::reencode(
            &mut h,
            PbfArgv::new("repack").input(&input).output(&output),
            "refusal",
        );
        assert_eq!(
            matches!(result, Ok(verify_reencode::Ran::TooLarge)),
            accepted
        );
    }
}

/// The suite runs both degrade checks under the same run directory.
fn judge_degrade_pair(h: &mut harness::Harness, exe: &Path, dir: &Path, input: &Path, low: &Path) {
    std::fs::write(dir.join("fault"), "").unwrap();
    h.run = run::Run::create(dir, exe, true).unwrap();
    h.run.watch(input).unwrap();
    h.run.watch(low).unwrap();
    let ordinary = super::judge(h, "degrade", |h| checks::degrade::run(h, input, true));
    assert_eq!(
        ordinary.status,
        harness::CheckStatus::Passed,
        "{ordinary:?}"
    );
    let locations = super::judge(h, "degrade -l", |h| {
        checks::degrade::run_locations(h, low, true)
    });
    assert_eq!(
        locations.status,
        harness::CheckStatus::Passed,
        "{locations:?}"
    );
}

/// Hold one encoding fault to the comparisons it must fail on every arm
/// (`arms`, the check's re-encodings in order with their block caps), or
/// to the error it must raise instead.
fn assert_encoding(fault: &str, arms: &[(&str, u32)], judged: &super::Judged) {
    let passes = fault.is_empty() || fault == "required-output";
    assert_eq!(
        judged.status,
        if passes {
            harness::CheckStatus::Passed
        } else {
            harness::CheckStatus::Failed
        },
        "fault={fault}: {judged:?}"
    );
    // The metadata and out-coords faults change one field class of the
    // output alone: every arm's element comparison must read it.
    let expected: Vec<String> = arms
        .iter()
        .flat_map(|(arm, cap)| match fault {
            "content" | "metadata" | "out-coords" => vec![format!("{arm} elements")],
            "layout" => vec![
                format!("{arm} indexdata against the verifier's blob decode"),
                format!("{arm} block sizes: blob 0 holds 0 nodes, cap {cap}"),
            ],
            "header" => vec![format!("{arm} header LocationsOnWays")],
            _ => vec![],
        })
        .collect();
    assert_eq!(judged.failed, expected, "fault={fault}");
    let error = match fault {
        "index" => Some(
            "pbfhogg check --index exited Some(0) with a report that says passed true with 1 violations",
        ),
        "refused" => {
            Some("as too large for the format: nothing was re-encoded, so nothing was verified")
        }
        "published" => Some("failed: exit status: 2: too large; lower --elements-per-blob"),
        _ => None,
    };
    match error {
        None => assert_eq!(judged.error, None, "fault={fault}"),
        Some(text) => assert!(
            judged.error.as_deref().is_some_and(|e| e.contains(text)),
            "fault={fault}: {judged:?}"
        ),
    }
}

pub fn tool(args: &[OsString]) -> i32 {
    let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
    let root = args
        .iter()
        .map(Path::new)
        .flat_map(Path::ancestors)
        .find(|p| p.file_name().is_some_and(|n| n == "encoding-fixture"))
        .unwrap();
    let fault = std::fs::read_to_string(root.join("fault")).unwrap_or_default();
    match args[0] {
        "fileinfo" => println!("{}", crate::fake_pbf::fileinfo(3, 0, 0)),
        "check" => {
            assert!(!args.contains(&"--direct-io"));
            println!(
                "{}",
                serde_json::json!({"index":{"blobs":1,"blobs_with_indexdata":1,"passed":true,"total_violations": if fault == "index" { 1 } else { 0 },"violations":[]}})
            );
        }
        "cat" if crate::fake_pbf::is_opl_cat(&args) => {
            // The element comparison's decodings, always with metadata and
            // way coordinates. A LocationsOnWays input decodes with a
            // coordinate its plain reference lacks; a pbfhogg output under
            // the content fault decodes one tag boundary shifted, which
            // osmium diff's checksum could not see.
            assert!(args.contains(&"opl,locations_on_ways=true"));
            let input = Path::new(args[1]);
            let name = input.file_name().unwrap().to_str().unwrap();
            // The metadata and out-coords faults alter one field class of
            // the output alone: a version, and an inline coordinate its
            // input lacks.
            let ours = name.starts_with("repack") || name.starts_with("degrade");
            let way =
                if (name == "low.pbf" && fault != "no-coords") || (ours && fault == "out-coords") {
                    "w1 v1 Nn1x1y1"
                } else {
                    "w1 v1 Nn1x y"
                };
            let tag = if fault == "content" && ours {
                "Tab=c"
            } else {
                "Ta=bc"
            };
            let version = if fault == "metadata" && ours { 2 } else { 1 };
            let output = args[args.iter().position(|a| *a == "-o").unwrap() + 1];
            let opl = format!("n1 v{version} {tag}\nn2 v1\nn3 v1\n{way}\n");
            std::fs::write(output, crate::fake_pbf::project(&args, &opl)).unwrap();
        }
        "cat" => {
            // The reference without way coordinates: plain PBF output.
            assert!(!args.iter().any(|a| a.contains("locations_on_ways")));
            let output = Path::new(args[args.iter().position(|a| *a == "-o").unwrap() + 1]);
            write_pbf(output, false, 3, &["Sort.Type_then_ID"], &[]);
        }
        "repack" | "degrade" => {
            assert!(args.contains(&"--staging-dir"));
            assert!(!args.contains(&"--tmp-dir"));
            let output = Path::new(args[args.iter().position(|a| *a == "--output").unwrap() + 1]);
            if ["refused", "published", "wrong-advice"].contains(&fault.as_str()) {
                if fault == "published" {
                    write_pbf(output, true, 3, &[], &[]);
                }
                eprintln!(
                    "too large{}",
                    if fault == "wrong-advice" {
                        ""
                    } else {
                        "; lower --elements-per-blob"
                    }
                );
                return 2;
            }
            assert!(args.contains(&"--direct-io"));
            assert!(args.contains(&"--force"));
            // The output claims what the faults ask of its header: a sort
            // feature among the required ones changes no optional claim.
            let required: &[&str] = if fault == "required-output" {
                &["Sort.Type_then_ID"]
            } else {
                &[]
            };
            let optional: &[&str] = if fault == "header" {
                &["LocationsOnWays"]
            } else {
                &[]
            };
            write_pbf(
                output,
                true,
                if fault == "layout" { 0 } else { 3 },
                required,
                optional,
            );
        }
        other => panic!("unexpected encoding protocol {other}: {args:?}"),
    }
    0
}

fn field(number: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&[number * 8 + 2, u8::try_from(bytes.len()).unwrap()]);
    out.extend_from_slice(bytes);
}

fn frame(kind: &[u8], index: &[u8], payload: &[u8], out: &mut Vec<u8>) {
    let mut header = Vec::new();
    field(1, kind, &mut header);
    if !index.is_empty() {
        field(2, index, &mut header);
    }
    header.extend_from_slice(&[24, u8::try_from(payload.len()).unwrap()]);
    out.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
    out.extend(header);
    out.extend_from_slice(payload);
}

/// A PBF of one raw data blob holding dense nodes 1, 2 and 3, its
/// indexdata (when `indexed`) claiming nodes 1..3 and `count` elements, its
/// header the given features: the verifier's own header, framing and
/// indexdata readers consume all of it.
fn write_pbf(path: &Path, indexed: bool, count: u64, required: &[&str], optional: &[&str]) {
    let mut out = Vec::new();
    let mut block = Vec::new();
    for feature in required {
        field(4, feature.as_bytes(), &mut block);
    }
    for feature in optional {
        field(5, feature.as_bytes(), &mut block);
    }
    let mut header = Vec::new();
    field(1, &block, &mut header);
    frame(b"OSMHeader", &[], &header, &mut out);
    let mut index = vec![1, 0];
    index.extend_from_slice(&1_i64.to_le_bytes());
    index.extend_from_slice(&3_i64.to_le_bytes());
    index.extend_from_slice(&count.to_le_bytes());
    // Dense ids 1, 2, 3: zigzag deltas 1, 1, 1 (each encoded as 2).
    let mut dense = Vec::new();
    field(1, &[2, 2, 2], &mut dense);
    let mut group = Vec::new();
    field(2, &dense, &mut group);
    let mut primitive = Vec::new();
    field(1, &[], &mut primitive);
    field(2, &group, &mut primitive);
    let mut data = Vec::new();
    field(1, &primitive, &mut data);
    frame(
        b"OSMData",
        if indexed { &index } else { &[] },
        &data,
        &mut out,
    );
    std::fs::write(path, out).unwrap();
}
