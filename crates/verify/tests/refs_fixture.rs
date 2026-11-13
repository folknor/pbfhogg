//! A controlled tool protocol for exercising the referenced-object check.
//! Objects are generated as OPL here; no downloaded input or installed tool.
use std::path::Path;

use crate::{
    harness, pbf_frames, reference, reference_cache, run, verify_getid_refs, verify_input_order,
};

const OBJECTS: &str = "n5 T x1 y1\nn6 Tname=pulled x2 y2\nn60 Tname=requested x3 y3\nw7 T Nn5,n6\nw50 T Nn60\nr1 T Mn5@,w7@,r2@\nr2 T Mr3@\nr3 T M\n";

pub fn driver(life: crate::process::Lifecycle, exe: &Path, dir: &Path) {
    let input = dir.join("input.opl");
    std::fs::write(&input, OBJECTS).unwrap();
    let run = run::Run::create(dir, exe, true).unwrap();
    let cache = reference_cache::ReferenceCache::new(dir, "refs-fixture".into(), false).unwrap();
    let mut h = harness::Harness::new(
        life,
        run,
        reference::Tools {
            osmium: exe.into(),
            osmosis: None,
            osmconvert: None,
        },
        true,
        cache,
    );
    h.run.watch(&input).unwrap();
    let claim = |max_id, count| {
        Some(pbf_frames::KindClaim {
            min_id: 1,
            max_id,
            count,
        })
    };
    let summary = pbf_frames::IndexSummary {
        kinds: [claim(60, 3), claim(50, 2), claim(3, 3)],
        required_features: vec![],
    };
    let order = verify_input_order::InputOrder {
        sorted: true,
        reference_input: input.clone(),
    };
    for (name, fault, expected) in [
        ("correct", "", harness::CheckStatus::Passed),
        ("one-hop", "one-hop", harness::CheckStatus::Failed),
        ("keep-tags", "keep-tags", harness::CheckStatus::Failed),
    ] {
        std::fs::write(input.with_extension("fault"), fault).unwrap();
        let status = h
            .run_check(name, |h| {
                let outdir = h.subdir(name)?;
                let missing = verify_getid_refs::run(
                    h,
                    &verify_getid_refs::RefsInput {
                        outdir: &outdir,
                        pbf: &input,
                        order: &order,
                        summary: &summary,
                        direct_io: true,
                    },
                )?;
                assert!(missing.is_empty(), "{missing:?}");
                Ok(None)
            })
            .unwrap();
        assert_eq!(status, expected, "{name}");
    }
}

/// Emulate only the declared invocations the production check must make.
/// Assert their arguments as well as supplying the requested output.
pub fn tool(args: &[std::ffi::OsString]) -> i32 {
    let text: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
    let output = |flag| Path::new(text[text.iter().position(|s| *s == flag).unwrap() + 1]);
    match text[0] {
        "getid" => {
            for id in ["r1", "w50", "n60"] {
                assert!(text.contains(&id), "{text:?}");
            }
            let ours = text.contains(&"--add-referenced");
            let input = Path::new(*text.iter().find(|s| s.ends_with("input.opl")).unwrap());
            assert!(input.is_file(), "{text:?}");
            if ours {
                assert!(text.contains(&"--direct-io"));
                assert!(text.contains(&"--staging-dir"));
                assert!(!text.contains(&"--tmp-dir"));
            } else {
                assert!(text.contains(&"-r"));
                assert!(text.contains(&"-v"));
            }
            let fault = std::fs::read_to_string(input.with_extension("fault")).unwrap();
            let strip = text.contains(&"--remove-tags") || text.contains(&"-t");
            let mut objects = OBJECTS.to_owned();
            if strip && !(ours && fault == "keep-tags") {
                objects = objects.replace("n6 Tname=pulled", "n6 T");
            }
            if ours && fault == "one-hop" {
                objects = objects.replace("r3 T M\n", "");
            }
            std::fs::write(output(if ours { "--output" } else { "-o" }), objects).unwrap();
        }
        "cat" => {
            assert!(text.contains(&"opl"));
            let input = std::fs::read_to_string(text[1]).unwrap();
            let objects = if text.contains(&"relation") {
                input
                    .lines()
                    .filter(|s| s.starts_with('r'))
                    .map(|s| format!("{s}\n"))
                    .collect()
            } else {
                input
            };
            std::fs::write(output("-o"), objects).unwrap();
        }
        "inspect" => {
            let report = r#"{"header":{"required_features":[],"optional_features":["Sort.Type_then_ID"]},"elements":{"nodes":3,"ways":2,"relations":3},"ordering":{"standard":true},"data":{"objects_ordered":true,"bbox":null,"timestamp":null}}"#;
            print!("{report}");
        }
        "diff" => {
            assert!(text.contains(&"--suppress-common"));
            if std::fs::read(text[2]).unwrap() != std::fs::read(text[3]).unwrap() {
                println!("*r3");
                return 1;
            }
        }
        _ => panic!("unexpected fixture command: {text:?}"),
    }
    0
}
