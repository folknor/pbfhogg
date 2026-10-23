//! Baseline CLI host-policy identity instrument.
//! First bless (baseline fa94c6c7, host bygg): 24.1 s wall for the three-run
//! matrix, well inside the 280 s ceiling. The two `_uring` scenarios are an
//! environment skip wherever `CliOutput::is_uring_unsupported` matches, the
//! same convention every other uring test in the suite follows: bygg's
//! `RLIMIT_MEMLOCK` hard limit is 8 MB against the writer's 16 MB of
//! registered buffers, and a seccomp sandbox denies `io_uring_setup`. A
//! skipped scenario writes no goldens and is named on stderr by both tests,
//! so a gate log never reads as full coverage; on a host where io_uring
//! works, bless there and the scenarios become mandatory by themselves.
//! Only the explicitly listed path, PID, inode, memory, timing and
//! inherited-descriptor-count fields are masked. The id-W<worker>- scratch routing component is also masked:
//! the dispatcher assigns the same blob to whichever worker is ready first.
//! inspect --nodes weighted bit-widths and their derived size/ratio estimates
//! are masked: each worker assembles its own 128-value groups, so assignment
//! of the three node blobs changes the group ranges. Counts, coordinate
//! ranges and histogram buckets remain compared.
//! Pool members are compared as multisets, not by scheduling order.
#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cognitive_complexity
)]

use crate::common::{self, cli::CliInvoker};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn fixtures() -> PathBuf {
    root().join("tests/fixtures/host_identity")
}
fn prerequisites() {
    assert!(
        std::thread::available_parallelism().unwrap().get() >= 4,
        "identity precondition: at least four CPUs including cgroup quota"
    );
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
        0
    );
    assert!(
        unsafe { limit.assume_init() }.rlim_max >= 4096,
        "identity precondition: hard RLIMIT_NOFILE >= 4096"
    );
}
fn shim() -> PathBuf {
    let dir = std::env::var_os("BROKKR_TEST_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/debug"));
    let path = dir.join("libprocess_trace_shim.so");
    assert!(
        path.exists(),
        "missing {}; run with --sweep all",
        path.display()
    );
    path
}
fn write_fixtures() {
    let dir = fixtures().join("inputs");
    std::fs::create_dir_all(&dir).unwrap();
    let mut nodes = common::generate_nodes(300, 1);
    let mut ways = common::generate_ways(40, 1, 5, 1);
    let mut relations = common::generate_relations(5, 1, 2, 1);
    for n in &mut nodes {
        if n.id % 10 == 0 {
            n.tags.push(("amenity", "bench"));
        }
    }
    for w in &mut ways {
        if w.id % 2 == 1 {
            w.tags.push(("highway", "residential"));
        }
    }
    for r in &mut relations {
        r.tags.push(("type", "multipolygon"));
    }
    for name in ["base", "changed"] {
        if name == "changed" {
            nodes.retain(|n| n.id != 7);
            nodes.extend(common::generate_nodes(1, 301));
            ways.retain(|w| w.id != 40);
        }
        let raw = dir.join(format!("{name}-noindex.osm.pbf"));
        common::write_multi_block_test_pbf(&raw, &nodes, &ways, &relations, 100);
        // The sync writer also emits indexdata; strip it explicitly so this
        // fixture exercises the unindexed failure boundary independently.
        let mut bytes = std::fs::read(&raw).unwrap();
        let blobs = common::adversarial::locate_blobs(&bytes).len();
        for index in 1..blobs {
            bytes = common::adversarial::mutate_blob_header_indexdata(&bytes, index, Vec::clear);
        }
        std::fs::write(&raw, bytes).unwrap();
        CliInvoker::new()
            .clean_env()
            .arg("cat")
            .arg(raw)
            .arg("-o")
            .arg(dir.join(format!("{name}.osm.pbf")))
            .assert_success();
    }
    crate::cli_apply_changes::write_crud_base(&dir.join("apply-base.osm.pbf"));
    crate::cli_apply_changes::write_osc(
        &dir.join("apply.osc.gz"),
        crate::cli_apply_changes::CRUD_OSC,
    );
    crate::cli_time_filter::write_history_input(&dir.join("history.osm.pbf"));
    crate::cli_time_filter::write_snapshot_input(&dir.join("snapshot.osm.pbf"));
    crate::geocode_index::write_synthetic_indexed_input(&dir.join("geocode.osm.pbf"));
    std::fs::write(dir.join("extract.json"), r#"{"directory":".","extracts":[{"output":"a.osm.pbf","bbox":[0,0,0.015,0.015]},{"output":"b.osm.pbf","bbox":[0.015,0.015,0.03,0.03]}]}"#).unwrap();
    let bad = common::adversarial::mutate_blob_header_indexdata(
        &std::fs::read(dir.join("base.osm.pbf")).unwrap(),
        1,
        |v| v.resize(30, 0),
    );
    std::fs::write(dir.join("bad-indexdata.osm.pbf"), bad).unwrap();
}

struct Scenario {
    name: &'static str,
    args: &'static str,
    exit: i32,
    limits: (u64, u64),
}
fn scenarios() -> Vec<Scenario> {
    let mut all = Vec::new();
    macro_rules! scenario {
        ($name:literal, $args:literal, $exit:literal) => {
            all.push(Scenario {
                name: $name,
                args: $args,
                exit: $exit,
                limits: (1024, 4096),
            });
        };
    }
    scenario!("cat_passthrough", "cat INPUT/base.osm.pbf -o OUTPUT", 0);
    scenario!(
        "cat_type_way",
        "cat INPUT/base.osm.pbf --type way -o OUTPUT",
        0
    );
    scenario!(
        "cat_clean_version",
        "cat INPUT/base.osm.pbf --clean version -o OUTPUT",
        0
    );
    scenario!(
        "cat_type_way_clean",
        "cat INPUT/base.osm.pbf --type way --clean version -o OUTPUT",
        0
    );
    scenario!(
        "cat_dedupe",
        "cat INPUT/base.osm.pbf INPUT/changed.osm.pbf --dedupe -o OUTPUT",
        0
    );
    scenario!("check_default", "check INPUT/base.osm.pbf", 0);
    scenario!("check_ids", "check INPUT/base.osm.pbf --ids", 0);
    scenario!("check_ids_full", "check INPUT/base.osm.pbf --ids --full", 0);
    scenario!("check_refs", "check INPUT/base.osm.pbf --refs", 0);
    scenario!("check_index", "check INPUT/base.osm.pbf --index", 0);
    scenario!(
        "check_index_refs",
        "check INPUT/base.osm.pbf --index --refs",
        0
    );
    scenario!(
        "degrade_strip_indexdata",
        "degrade INPUT/base.osm.pbf --strip-indexdata -o OUTPUT",
        0
    );
    scenario!(
        "degrade_strip_locations",
        "degrade INPUT/base.osm.pbf --strip-locations -o OUTPUT",
        0
    );
    scenario!(
        "repack",
        "repack INPUT/base.osm.pbf --elements-per-blob 100 -o OUTPUT",
        0
    );
    scenario!("renumber", "renumber INPUT/base.osm.pbf -o OUTPUT", 0);
    scenario!("sort", "sort INPUT/base.osm.pbf -o OUTPUT", 0);
    scenario!(
        "time_filter_snapshot",
        "time-filter INPUT/snapshot.osm.pbf -o OUTPUT 250",
        0
    );
    scenario!(
        "time_filter_history",
        "time-filter INPUT/history.osm.pbf -o OUTPUT 250",
        0
    );
    scenario!(
        "geocode",
        "build-geocode-index INPUT/geocode.osm.pbf --output-dir OUTPUT",
        0
    );
    scenario!(
        "altw_sparse",
        "add-locations-to-ways INPUT/base.osm.pbf --index-type sparse -o OUTPUT",
        0
    );
    scenario!(
        "altw_external",
        "add-locations-to-ways INPUT/base.osm.pbf --index-type external -o OUTPUT",
        0
    );
    scenario!(
        "altw_auto",
        "add-locations-to-ways INPUT/base.osm.pbf --index-type auto -o OUTPUT",
        0
    );
    scenario!(
        "diff_text",
        "diff INPUT/base.osm.pbf INPUT/changed.osm.pbf -c",
        1
    );
    scenario!(
        "diff_osc",
        "diff INPUT/base.osm.pbf INPUT/changed.osm.pbf --format osc -o OUTPUT.osc.gz",
        0
    );
    scenario!(
        "apply_changes",
        "apply-changes INPUT/apply-base.osm.pbf INPUT/apply.osc.gz -o OUTPUT --force",
        0
    );
    scenario!(
        "apply_changes_uring",
        "apply-changes INPUT/apply-base.osm.pbf INPUT/apply.osc.gz -o OUTPUT --force --io-uring",
        0
    );
    scenario!(
        "sort_direct_io",
        "sort INPUT/base.osm.pbf --direct-io -o OUTPUT",
        0
    );
    scenario!(
        "sort_uring",
        "sort INPUT/base.osm.pbf --io-uring -o OUTPUT",
        0
    );
    scenario!(
        "bad_indexdata",
        "cat INPUT/bad-indexdata.osm.pbf --type way --force -o OUTPUT",
        0
    );
    scenario!(
        "extract_complete",
        "extract INPUT/base.osm.pbf -b 0,0,0.015,0.015 -o OUTPUT",
        0
    );
    scenario!(
        "extract_simple",
        "extract INPUT/base.osm.pbf -b 0,0,0.015,0.015 --simple -o OUTPUT",
        0
    );
    scenario!(
        "extract_smart",
        "extract INPUT/base.osm.pbf -b 0,0,0.015,0.015 --smart -o OUTPUT",
        0
    );
    scenario!(
        "extract_multi",
        "extract INPUT/base.osm.pbf -c INPUT/extract.json -d OUTPUT",
        0
    );
    scenario!(
        "tags_filter",
        "tags-filter INPUT/base.osm.pbf highway -o OUTPUT",
        0
    );
    scenario!(
        "tags_filter_omit_ref",
        "tags-filter INPUT/base.osm.pbf -R highway -o OUTPUT",
        0
    );
    scenario!(
        "tags_filter_osc",
        "tags-filter INPUT/apply.osc.gz --input-kind osc name -o OUTPUT.osc.gz",
        0
    );
    scenario!("tags_count", "inspect tags INPUT/base.osm.pbf", 0);
    scenario!("getid", "getid INPUT/base.osm.pbf n10 w3 r1 -o OUTPUT", 0);
    scenario!(
        "getid_add_referenced",
        "getid INPUT/base.osm.pbf -r w3 -o OUTPUT",
        0
    );
    scenario!(
        "removeid",
        "getid INPUT/base.osm.pbf --invert n10 -o OUTPUT",
        0
    );
    scenario!(
        "getparents",
        "getparents INPUT/base.osm.pbf n1 -o OUTPUT",
        0
    );
    scenario!(
        "export_nodes",
        "export INPUT/base.osm.pbf --type node -o OUTPUT",
        0
    );
    scenario!("inspect", "inspect INPUT/base.osm.pbf", 0);
    scenario!("inspect_nodes", "inspect INPUT/base.osm.pbf --nodes", 0);
    scenario!("inspect_show", "inspect INPUT/base.osm.pbf --show w3", 0);
    scenario!(
        "inspect_get",
        "inspect INPUT/base.osm.pbf --get header.writing_program",
        0
    );
    scenario!(
        "merge_changes",
        "merge-changes INPUT/apply.osc.gz -o OUTPUT.osc.gz",
        0
    );
    scenario!(
        "merge_changes_j2",
        "merge-changes INPUT/apply.osc.gz -j 2 -o OUTPUT.osc.gz",
        0
    );
    scenario!(
        "bench_read_sequential",
        "bench-read INPUT/base.osm.pbf --mode sequential",
        0
    );
    scenario!(
        "bench_read_parallel",
        "bench-read INPUT/base.osm.pbf --mode parallel",
        0
    );
    scenario!(
        "bench_read_pipelined",
        "bench-read INPUT/base.osm.pbf --mode pipelined",
        0
    );
    scenario!(
        "bench_read_blobreader",
        "bench-read INPUT/base.osm.pbf --mode blobreader",
        0
    );
    scenario!(
        "fail_degrade_no_flag",
        "degrade INPUT/base.osm.pbf -o OUTPUT",
        2
    );
    scenario!(
        "fail_repack_zero_cap",
        "repack INPUT/base.osm.pbf --elements-per-blob 0 -o OUTPUT",
        2
    );
    scenario!(
        "fail_cat_clean_unindexed",
        "cat INPUT/base-noindex.osm.pbf --clean version -o OUTPUT",
        2
    );
    all.push(Scenario {
        name: "altw_external_at_cap",
        args: "add-locations-to-ways INPUT/base.osm.pbf --index-type external -o OUTPUT",
        exit: 0,
        limits: (4096, 4096),
    });
    all.push(Scenario {
        name: "altw_external_nofile_too_low",
        args: "add-locations-to-ways INPUT/base.osm.pbf --index-type external -o OUTPUT",
        exit: 2,
        limits: (300, 300),
    });
    all.sort_by_key(|scenario| scenario.name.ends_with("_uring"));
    all
}

fn normalise(text: &str, dir: &Path) -> String {
    use regex::Regex;
    let mut s = text.replace(dir.to_str().unwrap(), "<DIR>");
    for (pattern, replacement) in [
        (
            r"((?:\.pbfhogg-[a-z-]+-|sparse-index-|derive-(?:creates|modifies|deletes)-|derive-par-(?:creates|modifies|deletes)-|diff-par-))\d+",
            "${1}<PID>",
        ),
        (r"/#\d+ \(deleted\)", "/#<INO> (deleted)"),
        (r"/id-W\d+-", "/id-W<WORKER>-"),
        (r"Done in [0-9.]+s", "Done in <T>s"),
        (r"elapsed_ms=\d+", "elapsed_ms=<T>"),
        (r"(?m)^(  (?:lat|lon): )\d+\.\d+ bits$", "${1}<BITS> bits"),
        (
            r"(?m)^  [0-9.]+ GB \(\d+ blocks x \([0-9.]+ \+ [0-9.]+\) bytes/block\)$",
            "  <SIZE> GB (<BLOCKS> blocks x (<SIZE> + <SIZE>) bytes/block)",
        ),
        (r"(?m)^(  ratio: )[0-9.]+%$", "${1}<RATIO>%"),
        // The descriptors a run inherits depend on whoever launched it.
        (
            r"\d+ (already )?open, \d+ available",
            "<FDS> ${1}open, <FDS> available",
        ),
        (r"ulimit -n \d+", "ulimit -n <FDS>"),
        (
            r"fd budget allows up to \d+ workers",
            "fd budget allows up to <FDS> workers",
        ),
    ] {
        s = Regex::new(pattern)
            .unwrap()
            .replace_all(&s, replacement)
            .into_owned();
    }
    let numbers = Regex::new(r"\d+(?:\.\d+)?").unwrap();
    let memory = Regex::new(r"of [0-9.]+ GB page-cache budget").unwrap();
    s.split_inclusive('\n')
        .map(|line| {
            if line.starts_with("auto-selected --index-type") {
                memory
                    .replace_all(line, "of <MEM> GB page-cache budget")
                    .into_owned()
            } else if line.starts_with("  Bytes:") || line.starts_with("  Blob sizes:") {
                numbers.replace_all(line, "<N>").into_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<String>()
}

fn canonical(raw: &str) -> String {
    let mut threads: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in raw.lines() {
        let (id, rest) = line.split_once(' ').unwrap();
        if rest != "S" && rest != "E" {
            threads
                .entry(id.to_owned())
                .or_default()
                .push(rest.to_owned());
        }
    }
    // The global pool may be initialised by any first-touch thread.
    let mut global = Vec::new();
    for records in threads.values_mut() {
        if let Some(at) = records.iter().position(|r| r == "V RAYON_NUM_THREADS 0") {
            let mut end = at + 1;
            while end < records.len()
                && (records[end].starts_with("V RAYON_RS_NUM_CPUS ")
                    || records[end].starts_with("A ")
                    || records[end].starts_with("C "))
            {
                end += 1;
            }
            global = records.drain(at..end).filter_map(|r| child(&r)).collect();
            records.insert(at, "G".into());
        }
    }
    let mut lines = Vec::new();
    tree("0", "0", &threads, &mut lines, &mut HashSet::new());
    if !global.is_empty() {
        pool("global", &global, &threads, &mut lines);
    }
    lines.join("\n") + "\n"
}
fn child(record: &str) -> Option<String> {
    let mut fields = record.split_whitespace();
    if fields.next() == Some("C") {
        let id = fields.next()?;
        if id != "-" {
            return Some(id.to_owned());
        }
    }
    None
}
fn flatten(
    id: &str,
    threads: &BTreeMap<String, Vec<String>>,
    records: &mut Vec<String>,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(id.to_owned()) {
        return;
    }
    if let Some(seq) = threads.get(id) {
        for record in seq {
            if let Some(c) = child(record) {
                records.push("C <child> 0".into());
                flatten(&c, threads, records, visited);
            } else {
                records.push(record.clone());
            }
        }
    }
}
fn pool(
    name: &str,
    members: &[String],
    threads: &BTreeMap<String, Vec<String>>,
    lines: &mut Vec<String>,
) {
    lines.push(format!("{name} pool {}", members.len()));
    let mut records = Vec::new();
    let mut visited = HashSet::new();
    for id in members {
        flatten(id, threads, &mut records, &mut visited);
    }
    records.sort();
    lines.extend(records.into_iter().map(|r| format!("{name} {r}")));
}
fn tree(
    id: &str,
    name: &str,
    threads: &BTreeMap<String, Vec<String>>,
    lines: &mut Vec<String>,
    visited: &mut HashSet<String>,
) {
    if !visited.insert(id.to_owned()) {
        return;
    }
    let Some(seq) = threads.get(id) else {
        return;
    };
    let mut at = 0;
    let mut index = 0;
    while at < seq.len() {
        if child(&seq[at]).is_some() {
            let mut members = Vec::new();
            while at < seq.len() {
                if let Some(c) = child(&seq[at]) {
                    members.push(c);
                    at += 1;
                } else {
                    break;
                }
            }
            let label = format!("{name}.{index}");
            index += members.len();
            if members.len() > 1 {
                lines.push(format!("{name} C {label} {}", members.len()));
                pool(&label, &members, threads, lines);
            } else {
                lines.push(format!("{name} C {label} 0"));
                tree(&members[0], &label, threads, lines, visited);
            }
        } else {
            lines.push(format!("{name} {}", seq[at]));
            at += 1;
        }
    }
}
fn digest(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}
fn metadata(info: &pbfhogg::Info<'_>) -> String {
    format!(
        "{:?}",
        (
            info.version(),
            info.milli_timestamp(),
            info.changeset(),
            info.uid(),
            info.user().map(|v| v.unwrap().to_owned()),
            info.visible()
        )
    )
}
fn pbf_content(path: &Path) -> String {
    use pbfhogg::Element;
    let reader = pbfhogg::ElementReader::from_path(path).unwrap();
    let mut text = format!("{:?}\n", reader.header());
    reader
        .for_each(|e| {
            let line = match e {
                Element::Node(n) => format!(
                    "n {} {:?} {:?} {} {}",
                    n.id(),
                    n.tags().collect::<Vec<_>>(),
                    metadata(&n.info()),
                    n.nano_lat(),
                    n.nano_lon()
                ),
                Element::DenseNode(n) => format!(
                    "n {} {:?} {:?} {} {}",
                    n.id(),
                    n.tags().collect::<Vec<_>>(),
                    n.info().map(|i| (
                        i.version(),
                        i.milli_timestamp(),
                        i.changeset(),
                        i.uid(),
                        i.user().unwrap().to_owned(),
                        i.visible()
                    )),
                    n.nano_lat(),
                    n.nano_lon()
                ),
                Element::Way(w) => format!(
                    "w {} {:?} {:?} {:?} {:?}",
                    w.id(),
                    w.tags().collect::<Vec<_>>(),
                    metadata(&w.info()),
                    w.refs().collect::<Vec<_>>(),
                    w.node_locations().collect::<Vec<_>>()
                ),
                Element::Relation(r) => format!(
                    "r {} {:?} {:?} {:?}",
                    r.id(),
                    r.tags().collect::<Vec<_>>(),
                    metadata(&r.info()),
                    r.members()
                        .map(|m| (m.id, m.role().unwrap().to_owned()))
                        .collect::<Vec<_>>()
                ),
                _ => panic!("unknown element kind in identity digest"),
            };
            text.push_str(&line);
            text.push('\n');
        })
        .unwrap();
    digest(text.as_bytes())
}
fn output_digests(path: &Path, variable: bool, pbf: bool) -> String {
    let mut files = Vec::new();
    if path.is_dir() {
        files.extend(std::fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
    } else if path.exists() {
        files.push(path.to_owned());
    }
    files.sort();
    let mut out = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_str().unwrap();
        let mut bytes = std::fs::read(&file).unwrap();
        if name.ends_with(".gz") {
            let mut decoded = Vec::new();
            flate2::read::MultiGzDecoder::new(&bytes[..])
                .read_to_end(&mut decoded)
                .unwrap();
            bytes = decoded;
        }
        if name.ends_with(".pbf") || (name == "output" && pbf) {
            out.push(format!("{name} content {}", pbf_content(&file)));
            if !variable {
                out.push(format!("{name} raw {}", digest(&bytes)));
            }
        } else {
            out.push(format!("{name} raw {}", digest(&bytes)));
        }
    }
    out.join("\n") + "\n"
}
/// `None` is the io_uring environment skip; every other outcome is a
/// capture or a panic.
fn capture(s: &Scenario) -> Option<BTreeMap<&'static str, String>> {
    use std::os::unix::process::ExitStatusExt;
    let runs = root().join("target/host-identity/run");
    std::fs::create_dir_all(&runs).unwrap();
    let dir = tempfile::Builder::new()
        .prefix(&format!("{}-", s.name))
        .tempdir_in(runs)
        .unwrap();
    let input = dir.path().join("inputs");
    std::fs::create_dir(&input).unwrap();
    for file in std::fs::read_dir(fixtures().join("inputs")).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), input.join(file.file_name())).unwrap();
    }
    let argv: Vec<_> = s
        .args
        .split_whitespace()
        .map(|arg| {
            arg.replace("INPUT", input.to_str().unwrap())
                .replace("OUTPUT", dir.path().join("output").to_str().unwrap())
        })
        .collect();
    let args = argv.join("\n");
    if s.name == "extract_multi" {
        std::fs::create_dir(dir.path().join("output")).unwrap();
    }
    let trace = dir.path().join("trace.txt");
    let output = CliInvoker::new()
        .clean_env()
        .child_limits(4, s.limits)
        .env("LD_PRELOAD", shim())
        .env("PROCESS_TRACE_PATH", &trace)
        .args(&argv)
        .run();
    let raw = std::fs::read_to_string(trace).unwrap();
    let rawdir = root().join("target/host-identity").join(s.name);
    std::fs::create_dir_all(&rawdir).unwrap();
    std::fs::write(rawdir.join("trace.txt"), &raw).unwrap();
    std::fs::write(rawdir.join("argv.txt"), &args).unwrap();
    std::fs::write(
        rawdir.join("exit.txt"),
        format!(
            "code={:?} signal={:?}\n",
            output.status.code(),
            output.status.signal()
        ),
    )
    .unwrap();
    std::fs::write(rawdir.join("stdout.txt"), &output.stdout).unwrap();
    std::fs::write(rawdir.join("stderr.txt"), &output.stderr).unwrap();
    if s.name.ends_with("_uring") && output.is_uring_unsupported() {
        // Same environment skip every other uring test in the suite
        // makes: bygg's RLIMIT_MEMLOCK hard limit is 8 MB (the writer
        // registers 16 MB) and a seccomp sandbox denies io_uring_setup.
        crate::common::skip(format!(
            "host-identity scenario {}: io_uring unavailable on this host: {}",
            s.name,
            output.stderr_str().lines().last().unwrap_or("")
        ));
        return None;
    }
    if !output.status.success() && s.name.ends_with("_direct_io") {
        assert!(
            !output.is_o_direct_unsupported(),
            "identity platform precondition failed for {}: {}",
            s.name,
            output.stderr_str()
        );
    }
    assert_eq!(
        output.status.code(),
        Some(s.exit),
        "{}: {}",
        s.name,
        output.stderr_str()
    );
    let outpath = if s.args.contains("OUTPUT.osc.gz") {
        dir.path().join("output.osc.gz")
    } else {
        dir.path().join("output")
    };
    let capture = BTreeMap::from([
        ("argv", normalise(&args, dir.path())),
        (
            "exit",
            format!(
                "code={:?} signal={:?}\n",
                output.status.code(),
                output.status.signal()
            ),
        ),
        ("stdout", normalise(&output.stdout_str(), dir.path())),
        ("stderr", normalise(&output.stderr_str(), dir.path())),
        ("trace", canonical(&normalise(&raw, dir.path()))),
        (
            "outputs",
            output_digests(
                &outpath,
                s.name.starts_with("apply_changes"),
                s.name != "export_nodes",
            ),
        ),
    ]);
    for (name, text) in &capture {
        std::fs::write(rawdir.join(format!("{name}.normalised.txt")), text).unwrap();
    }
    std::fs::write(rawdir.join("stdout.txt"), &output.stdout).unwrap();
    std::fs::write(rawdir.join("stderr.txt"), &output.stderr).unwrap();
    Some(capture)
}
fn controls() {
    let trace = |name: &str| {
        std::fs::read_to_string(fixtures().join("golden").join(name).join("trace.txt")).unwrap()
    };
    let cat = trace("cat_clean_version");
    assert!(cat.find("0 M -8 2 1").unwrap() < cat.find("0 C ").unwrap());
    let ext = trace("altw_external");
    assert!(ext.contains("GR 7 "));
    assert!(ext.contains("SR 7 4096 4096 "));
    let cap = trace("altw_external_at_cap");
    assert!(cap.contains("GR 7 "));
    assert!(!cap.contains("SR "));
    assert!(
        trace("time_filter_history")
            .lines()
            .any(|l| l.contains(" F ") && l.ends_with(" 4 0"))
    );
    assert!(trace("cat_passthrough").contains("V BROKKR_MARKER_FIFO 0"));
    assert!(cat.contains(" A "));
    assert!(ext.contains(" A "));
}
fn assert_capture(expected: &str, actual: &str, scenario: &str, artifact: &str) {
    if expected != actual {
        let before: Vec<_> = expected.lines().collect();
        let after: Vec<_> = actual.lines().collect();
        let line = (0..before.len().max(after.len()))
            .find(|&i| before.get(i) != after.get(i))
            .unwrap();
        panic!(
            "{scenario} {artifact} differs at line {}: expected {:?}, actual {:?}",
            line + 1,
            before.get(line),
            after.get(line)
        );
    }
}

mod tier3 {
    use super::*;
    #[test]
    #[ignore]
    fn host_identity_bless_goldens() {
        let start = std::time::Instant::now();
        prerequisites();
        write_fixtures();
        let mut captures = Vec::new();
        let mut skipped = Vec::new();
        for scenario in scenarios() {
            let Some(first) = capture(&scenario) else {
                skipped.push(scenario.name);
                continue;
            };
            for _ in 0..2 {
                let next = capture(&scenario).expect("a scenario that ran once runs again");
                for (name, expected) in &first {
                    assert_capture(expected, &next[name], scenario.name, name);
                }
            }
            captures.push((scenario.name, first));
        }
        // A platform failure must never leave a partial set of blessed goldens.
        for (scenario, capture) in captures {
            let dir = fixtures().join("golden").join(scenario);
            std::fs::create_dir_all(&dir).unwrap();
            for (name, text) in capture {
                std::fs::write(dir.join(format!("{name}.txt")), text).unwrap();
            }
        }
        controls();
        eprintln!(
            "identity bless wall: {:?}; scenarios skipped (no goldens written): {skipped:?}",
            start.elapsed()
        );
    }
    #[test]
    #[ignore]
    fn host_identity_matches_goldens() {
        prerequisites();
        controls();
        let mut skipped = Vec::new();
        for scenario in scenarios() {
            let Some(actual) = capture(&scenario) else {
                skipped.push(scenario.name);
                continue;
            };
            for (name, actual) in actual {
                let expected = std::fs::read_to_string(
                    fixtures()
                        .join("golden")
                        .join(scenario.name)
                        .join(format!("{name}.txt")),
                )
                .unwrap();
                assert_capture(&expected, &actual, scenario.name, name);
            }
        }
        eprintln!("identity compare: scenarios skipped: {skipped:?}");
    }
}
