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
        reference::Tools {
            osmium: exe.into(),
            osmosis: None,
        },
        false,
        cache,
    );
    for fault in [
        "",
        "required-output",
        "content",
        "index",
        "layout",
        "header",
        "refused",
        "published",
    ] {
        std::fs::write(dir.join("fault"), fault).unwrap();
        for check in [
            checks::repack::run as checks::ReadCheck,
            checks::degrade::run,
        ] {
            h.run = run::Run::create(&dir, exe, true).unwrap();
            h.run.watch(&input).unwrap();
            let status = h.run_check("encoding", |h| check(h, &input, true)).unwrap();
            assert_eq!(
                status,
                if fault.is_empty() || fault == "required-output" {
                    harness::CheckStatus::Passed
                } else {
                    harness::CheckStatus::Failed
                },
                "fault={fault}"
            );
        }
    }
    // A LocationsOnWays input: the reference is osmium cat without
    // coordinates, and the coverage diff must find a way that lost them.
    // Requiring coordinates fails on an input that declares none, or whose
    // coverage diff finds none; without the requirement that is a gap.
    let low = dir.join("low.pbf");
    write_pbf(&low, false, 3, &["Sort.Type_then_ID"], &["LocationsOnWays"]);
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
        let status = h.run_check("degrade", |h| check(h, path, true)).unwrap();
        assert_eq!(
            status == harness::CheckStatus::Passed,
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
        "diff" => {
            assert!(args.contains(&"opl,locations_on_ways=true"));
            // The coverage diff: the input with coordinates against the
            // reference osmium cat wrote without them.
            if args.iter().any(|a| a.ends_with("reference-plain.osm.pbf"))
                && args.iter().any(|a| a.ends_with("low.pbf"))
            {
                if fault == "no-coords" {
                    eprintln!("Summary: left=0 right=0 same=3 different=0");
                    return 0;
                }
                eprintln!("Summary: left=0 right=0 same=2 different=1");
                println!("-w1 v1 Nn1x1y1\n+w1 v1 Nn1");
                return 1;
            }
            if fault == "content" {
                eprintln!("Summary: left=0 right=0 same=2 different=1");
                println!("-n1 v1\n+n1 v2");
                return 1;
            }
            eprintln!("Summary: left=0 right=0 same=3 different=0");
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
