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
    write_pbf(&input, false, 3);
    let run = run::Run::create(&dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(&dir, "encoding".into(), true).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools {
            osmium: exe.into(),
            osmosis: None,
            osmconvert: None,
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
        "fileinfo" => println!(
            r#"{{"data":{{"objects_ordered":false,"count":{{"nodes":3,"ways":0,"relations":0}},"minid":{{"nodes":1,"ways":0,"relations":0}},"maxid":{{"nodes":3,"ways":0,"relations":0}}}}}}"#
        ),
        "inspect" => println!(
            "{}",
            serde_json::json!({
                "header": {"required_features": if args.iter().any(|a| a.ends_with("/input.pbf")) || fault == "required-output" { vec!["Sort.Type_then_ID"] } else { vec![] }, "optional_features": if fault == "header" && !args.iter().any(|a| a.ends_with("/input.pbf")) { vec!["LocationsOnWays"] } else { vec![] }},
                "indexed": true, "elements": {"nodes":3,"tagged_nodes":0,"ways":0,"relations":0,"total":3}, "ordering": {"standard":true}
            })
        ),
        "check" => {
            assert!(!args.contains(&"--direct-io"));
            println!(
                "{}",
                serde_json::json!({"index":{"blobs":1,"blobs_with_indexdata":1,"passed":true,"total_violations": if fault == "index" { 1 } else { 0 },"violations":[]}})
            );
        }
        "diff" => {
            assert!(args.contains(&"opl,locations_on_ways=true"));
            if fault == "content" {
                eprintln!("Summary: left=0 right=0 same=2 different=1");
                println!("-n1 v1\n+n1 v2");
                return 1;
            }
            eprintln!("Summary: left=0 right=0 same=3 different=0");
        }
        "repack" | "degrade" => {
            assert!(args.contains(&"--staging-dir"));
            assert!(!args.contains(&"--tmp-dir"));
            let output = Path::new(args[args.iter().position(|a| *a == "--output").unwrap() + 1]);
            if ["refused", "published", "wrong-advice"].contains(&fault.as_str()) {
                if fault == "published" {
                    write_pbf(output, true, 3);
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
            write_pbf(output, true, if fault == "layout" { 0 } else { 3 });
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

fn write_pbf(path: &Path, indexed: bool, count: u64) {
    let mut out = Vec::new();
    // Raw empty HeaderBlock. The data is never decoded by this protocol;
    // production framing and claim readers still consume its real framing.
    frame(b"OSMHeader", &[], &[10, 0], &mut out);
    let mut index = vec![1, 0];
    index.extend_from_slice(&1_i64.to_le_bytes());
    index.extend_from_slice(&3_i64.to_le_bytes());
    index.extend_from_slice(&count.to_le_bytes());
    frame(
        b"OSMData",
        if indexed { &index } else { &[] },
        &[10, 0],
        &mut out,
    );
    std::fs::write(path, out).unwrap();
}
