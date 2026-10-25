//! The output contract, end to end through the binary, for the properties
//! that are not the same on every command: the aliasing check, error wording for bad inputs, the failures
//! that land after a writer has finished, and who performs the `fsync`.
//!
//! The properties every writing subcommand shares - an existing output is
//! refused before any work, `--overwrite` and `-O` replace it, a symlink is
//! refused, a failed run leaves nothing, a successful run leaves only its
//! outputs, `--tmp-dir` is checked first and holds the scratch - are one
//! table in `cli_output_contract`, driven by the binary's own list of
//! subcommands.
//!
//! The publication mechanism - staging, refusal, exclusive publication,
//! group rollback, every injected fault - is proven on the library type in
//! `src/output.rs`. Forced mid-run failures of real commands are in the
//! fault tests (`fault_apply_changes`, `fault_parallel_writer`,
//! `fault_geocode_pass3`) and `renumber_external`, each asserting that no
//! file appears at the destination.
//!
//! Here, through the CLI: `cat` with a corrupt second input fails after its
//! writer opened, and `add-locations-to-ways` refuses a complete output
//! after its writer finished; both must leave a pre-existing destination
//! byte-identical.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use crate::invoker::CliInvoker;
use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
use pbfhogg::writer::{Compression, PbfWriter};

fn write_fixture(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 100_000_000, 100_000_000, [("amenity", "cafe")], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    writer.flush().unwrap();
}

/// A PBF truncated inside a blob. Framing is broken, so this fails in the
/// header preflight; used only where the question is what the *message*
/// says.
fn write_truncated(path: &Path, source: &Path) {
    let mut bytes = std::fs::read(source).unwrap();
    let keep = bytes.len() - 5;
    bytes.truncate(keep);
    std::fs::write(path, bytes).unwrap();
}

fn scratch_leftovers(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| n.to_string_lossy().starts_with(".pbfhogg-tmp-"))
        .collect()
}

/// Run a command that should succeed, and assert its output landed at the
/// name the user gave with no scratch file left beside it.
///
/// This is the test that bites on the realistic wiring mistakes: a call
/// site wrapped in the guard but never committed leaves the destination
/// missing, and a guard that forgets to remove or rename its scratch
/// leaves a stray `.pbfhogg-tmp-*` behind. Both fail here loudly.
fn assert_output_published(dir: &Path, dest: &Path, args: &[&str]) {
    let out = CliInvoker::new().args(args).run();
    assert!(
        out.status.success(),
        "command failed; stderr:\n{}",
        out.stderr_str()
    );
    assert!(
        dest.exists(),
        "output never appeared at the path the user named: {}",
        dest.display()
    );
    let len = std::fs::metadata(dest).unwrap().len();
    assert!(len > 0, "output at {} is empty", dest.display());
    let strays = scratch_leftovers(dir);
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// A nonexistent input must be reported as a missing input, by name. The
/// up-front aliasing check canonicalizes every input path, and used to
/// surface a nonexistent one as a bare `No such file or directory` from
/// the check itself - before the command ran, and without naming the file.
#[test]
fn missing_input_is_reported_as_a_missing_input() {
    let dir = crate::common::test_scratch();
    let missing = dir.path().join("nowhere.osm.pbf");
    let dest = dir.path().join("out.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "sort",
            missing.to_str().unwrap(),
            "-o",
            dest.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("nowhere.osm.pbf"),
        "missing-input error does not name the input; stderr:\n{stderr}"
    );
}

/// The aliasing check itself must still fire - canonicalizing lazily must
/// not have turned it off for inputs that do exist.
#[test]
fn input_output_alias_is_still_rejected() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_fixture(&input);
    let out = CliInvoker::new()
        .args([
            "sort",
            input.to_str().unwrap(),
            "--force",
            "-o",
            input.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    assert!(
        out.stderr_str().contains("aliases input"),
        "aliasing check did not fire; stderr:\n{}",
        out.stderr_str()
    );
}

/// Publication swaps a geocode index directory whole and deletes the one it
/// replaced, so an input inside it - or a symlink inside it naming an input
/// elsewhere - would be deleted by a successful `--overwrite` run. Both are
/// refused before any work, even though the directory is a replaceable index.
#[test]
fn a_geocode_index_directory_holding_an_input_is_refused() {
    let dir = crate::common::test_scratch();
    let idx = dir.path().join("idx");
    std::fs::create_dir(&idx).unwrap();
    // An index, as far as replacement is concerned.
    std::fs::write(idx.join("geocode_header.bin"), b"").unwrap();
    let inside = idx.join("in.osm.pbf");
    write_fixture(&inside);
    let outside = dir.path().join("source.osm.pbf");
    write_fixture(&outside);
    let link = idx.join("link.osm.pbf");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    for input in [&inside, &link] {
        let out = CliInvoker::new()
            .arg("build-geocode-index")
            .arg(input)
            .arg("--allow-unindexed")
            .arg("--overwrite")
            .arg("--output-dir")
            .arg(&idx)
            .run();
        let stderr = out.stderr_str();
        assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
        assert!(
            stderr.contains("contains input"),
            "containment check did not fire; stderr:\n{stderr}"
        );
        // Nothing was deleted, staged or made as scratch, here or inside.
        let names = |d: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(dir.path()), ["idx", "source.osm.pbf"]);
        assert_eq!(
            names(&idx),
            ["geocode_header.bin", "in.osm.pbf", "link.osm.pbf"]
        );
    }
}

/// A framing error names the input it came from and the blob it sits in.
/// On a multi-input command a byte offset alone cannot say which stream it
/// indexes.
#[test]
fn framing_error_names_the_input_file() {
    let dir = crate::common::test_scratch();
    let good = dir.path().join("in.osm.pbf");
    let bad = dir.path().join("corrupt.osm.pbf");
    write_fixture(&good);
    write_truncated(&bad, &good);
    let dest = dir.path().join("out.osm.pbf");
    let out = CliInvoker::new()
        .args([
            "cat",
            good.to_str().unwrap(),
            bad.to_str().unwrap(),
            "--clean",
            "version",
            "-o",
            dest.to_str().unwrap(),
        ])
        .run();
    assert!(!out.status.success());
    let stderr = out.stderr_str();
    assert!(
        stderr.contains("corrupt.osm.pbf"),
        "error does not name the failing input; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("blob "),
        "error does not name the failing blob; stderr:\n{stderr}"
    );
}

/// The survival property, per-command, on the one multi-input path where a
/// CLI-reachable failure genuinely lands *after* the writer opened: `cat`
/// re-encoding two inputs writes the first one out before it ever reads the
/// second, so a corrupt second input fails mid-stream with the destination
/// already opened under the old shape.
///
/// `--overwrite` is given, so the run gets past the refusal and genuinely
/// fails mid-stream: writing to the destination instead of the staging file
/// would leave the seeded file gone.
#[test]
fn cat_failure_leaves_preexisting_destination_intact() {
    let dir = crate::common::test_scratch();
    let good = dir.path().join("in.osm.pbf");
    let bad = dir.path().join("corrupt.osm.pbf");
    write_fixture(&good);
    write_truncated(&bad, &good);
    let dest = dir.path().join("precious.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();
    let out = CliInvoker::new()
        .args([
            "cat",
            good.to_str().unwrap(),
            bad.to_str().unwrap(),
            "--clean",
            "version",
            "-o",
            dest.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert!(!out.status.success(), "corrupt input should fail the run");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"user data",
        "a failed run destroyed the pre-existing destination"
    );
    let strays = scratch_leftovers(dir.path());
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// A PBF whose single way references a node that is not in the file.
/// `add-locations-to-ways` writes a complete output and only then refuses,
/// which is the post-writer CLI failure this file otherwise has no way to
/// produce.
fn write_dangling_ref_fixture(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
    writer
        .write_header(&HeaderBuilder::new().build().unwrap())
        .unwrap();
    let mut builder = BlockBuilder::new();
    builder.add_node(1, 100_000_000, 100_000_000, [("amenity", "cafe")], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    // Node 999 is absent, so the run fails its missing-locations policy.
    builder.add_way(10, [("highway", "primary")], &[1, 999], None);
    if let Some(bytes) = builder.take().unwrap() {
        writer.write_primitive_block(bytes).unwrap();
    }
    writer.flush().unwrap();
}

/// The single-input, post-writer case: `add-locations-to-ways` produces a
/// complete PBF and only then refuses it, so the guard is the only thing
/// standing between a failed run and a destroyed destination.
///
/// Unlike a truncated-input fixture, this genuinely fails after the writer
/// closed - point the dispatch site at the destination instead of the
/// scratch sibling and this test reports `precious.osm.pbf` replaced by a
/// PBF full of fabricated `(0, 0)` vertices.
#[test]
fn altw_missing_nodes_leave_preexisting_destination_intact() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_dangling_ref_fixture(&input);
    let dest = dir.path().join("precious.osm.pbf");
    std::fs::write(&dest, b"user data").unwrap();

    let out = CliInvoker::new()
        .args([
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "--force",
            "-o",
            dest.to_str().unwrap(),
            "--overwrite",
        ])
        .run();
    assert!(
        !out.status.success(),
        "an unresolvable way node ref must fail the run; stderr:\n{}",
        out.stderr_str()
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        b"user data",
        "a failed run destroyed the pre-existing destination"
    );
    let strays = scratch_leftovers(dir.path());
    assert!(strays.is_empty(), "scratch file leaked: {strays:?}");
}

/// Passing counterpart, so the test above is not satisfied by a command
/// that never publishes anything: the same fixture with the policy waived
/// replaces the destination through the guard, leaving no scratch behind.
#[test]
fn altw_publishes_through_the_guard() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_dangling_ref_fixture(&input);
    let dest = dir.path().join("out.osm.pbf");
    assert_output_published(
        dir.path(),
        &dest,
        &[
            "add-locations-to-ways",
            input.to_str().unwrap(),
            "--ignore-missing-nodes",
            "--force",
            "-o",
            dest.to_str().unwrap(),
        ],
    );
}

/// A usage error keeps clap's own exit code and its own message. The switch
/// from `Cli::parse` to a fallible parse exists to drop the hotpath guard
/// on the way out, and must be invisible otherwise - so this is a guard
/// against the change, not a demonstration of it.
#[test]
fn usage_error_keeps_claps_exit_code_and_message() {
    let out = CliInvoker::new().args(["sort", "--no-such-flag"]).run();
    assert_eq!(
        out.status.code(),
        Some(2),
        "usage error should exit 2; stderr:\n{}",
        out.stderr_str()
    );
    assert!(
        out.stderr_str().contains("unexpected argument"),
        "usage error lost clap's message; stderr:\n{}",
        out.stderr_str()
    );
}

/// `--help` is also an `Err` from `try_parse`, with exit code 0 and its
/// text on stdout. Getting that wrong would turn every `--help` into a
/// failure, which no other test here would notice.
#[test]
fn help_exits_zero_on_stdout() {
    let out = CliInvoker::new().args(["--help"]).run();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("OpenStreetMap PBF toolkit"),
        "--help text did not reach stdout"
    );
}

// ---------------------------------------------------------------------------
// Who syncs: publication, never the writer
// ---------------------------------------------------------------------------
//
// Command-layer PBF writers finish their files and leave the `fsync` to
// publication (ADR-0017). Each run below arms the sidecar marker stream at a
// regular file and reads two counters back: `writer_sync_all_calls`, every
// fsync the process recorded, and `publish_sync_calls`, the share recorded
// inside the publication span. Equal and at the expected count means the
// writer synced nothing and publication synced every staged file plus each
// parent once. A writer that went back to syncing at its own end of stream
// makes the total exceed the publication share; a publication that stopped
// syncing the file makes both fall short.

/// The value of the last `@name=` counter in a marker stream.
fn counter(events: &str, name: &str) -> Option<i64> {
    let needle = format!(" @{name}=");
    events
        .lines()
        .filter_map(|line| line.split_once(&needle).map(|(_, value)| value))
        .next_back()
        .map(|value| value.parse().unwrap())
}

/// Run the CLI with the marker stream armed; `None` when the run reports
/// that the host lacks what `args` asked for (O_DIRECT, io_uring).
fn run_with_markers(dir: &Path, label: &str, args: &[&str]) -> Option<String> {
    let markers = dir.join(format!("{label}.markers"));
    std::fs::write(&markers, []).unwrap();
    let out = CliInvoker::new()
        .env("BROKKR_MARKER_FIFO", &markers)
        .args(args)
        .run();
    if out.is_o_direct_unsupported() || out.is_uring_unsupported() {
        crate::common::skip(format!("{label}: the host lacks the requested I/O backend"));
        return None;
    }
    assert!(out.status.success(), "{label}: {}", out.stderr_str());
    Some(std::fs::read_to_string(&markers).unwrap())
}

#[track_caller]
fn assert_publication_did_every_sync(events: &str, label: &str, expected: i64) {
    let total = counter(events, "writer_sync_all_calls");
    let published = counter(events, "publish_sync_calls");
    assert_eq!(
        published,
        Some(expected),
        "{label}: syncs inside the publication span\n{events}"
    );
    assert_eq!(
        total,
        Some(expected),
        "{label}: every sync of the run, writer included\n{events}"
    );
    assert!(
        counter(events, "publish_sync_ns").is_some_and(|ns| ns >= 0),
        "{label}: publication's sync time\n{events}"
    );
    let start = events.find(" PUBLISH_START").expect("PUBLISH_START");
    let end = events.find(" PUBLISH_END").expect("PUBLISH_END");
    assert!(start < end, "{label}\n{events}");
}

fn empty_osc(dir: &Path) -> std::path::PathBuf {
    let osc = dir.join("empty.osc");
    std::fs::write(&osc, b"<osmChange version=\"0.6\"></osmChange>").unwrap();
    osc
}

#[test]
fn pbf_writers_leave_every_sync_to_publication() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let osc = empty_osc(dir.path());
    let (input, osc) = (input.to_str().unwrap(), osc.to_str().unwrap());
    let cat_out = dir.path().join("cat.osm.pbf");
    let apply_out = dir.path().join("apply.osm.pbf");
    // The buffered backend (`cat`) and the parallel one (apply-changes'
    // default): one staged file and its parent each.
    for (label, args) in [
        ("cat", vec!["cat", input, "-o", cat_out.to_str().unwrap()]),
        (
            "apply-changes",
            vec![
                "apply-changes",
                input,
                osc,
                "-o",
                apply_out.to_str().unwrap(),
            ],
        ),
    ] {
        let events = run_with_markers(dir.path(), label, &args).unwrap();
        assert_publication_did_every_sync(&events, label, 2);
    }
}

/// `extract --config` writes every region through a synchronous writer and
/// publishes them as one group: N staged files and their shared parent. A
/// replaced member also makes its backup durable before the swap (its
/// container and the parent), two more each.
#[test]
fn a_group_publication_does_every_sync_once() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    crate::common::write_base_pbf(&input);
    let out_dir = dir.path().join("out");
    std::fs::create_dir(&out_dir).unwrap();
    let config = dir.path().join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"directory":"{}","extracts":[
                {{"output":"a.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}},
                {{"output":"b.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}},
                {{"output":"c.osm.pbf","bbox":[0.0,0.0,1.0,1.0]}}]}}"#,
            out_dir.display()
        ),
    )
    .unwrap();
    let args = [
        "extract",
        input.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--simple",
    ];
    let events = run_with_markers(dir.path(), "fresh", &args).unwrap();
    assert_publication_did_every_sync(&events, "fresh", 3 + 1);

    // Replace all three: each backup adds its container and the parent.
    let mut replace = args.to_vec();
    replace.push("--overwrite");
    let events = run_with_markers(dir.path(), "replace", &replace).unwrap();
    assert_publication_did_every_sync(&events, "replace", 3 + 1 + 2 * 3);
    // Staging and the three backups are all gone once the parent synced.
    let left: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".pbfhogg-"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[cfg(any(feature = "linux-direct-io", feature = "linux-io-uring"))]
mod platform {
    use super::*;

    /// The O_DIRECT and io_uring backends leave the sync to publication
    /// too: bypassing the page cache is not stable storage, and publication
    /// syncs their files like any other.
    #[test]
    fn direct_and_uring_writers_leave_every_sync_to_publication() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        crate::common::write_base_pbf(&input);
        let osc = empty_osc(dir.path());
        let (input, osc) = (input.to_str().unwrap(), osc.to_str().unwrap());
        let mut flags: Vec<&str> = Vec::new();
        #[cfg(feature = "linux-direct-io")]
        flags.push("--direct-io");
        #[cfg(feature = "linux-io-uring")]
        flags.push("--io-uring");
        for flag in flags {
            let out = dir.path().join(format!("apply{flag}.osm.pbf"));
            let args = [
                "apply-changes",
                input,
                osc,
                "-o",
                out.to_str().unwrap(),
                flag,
            ];
            let Some(events) = run_with_markers(dir.path(), flag, &args) else {
                continue;
            };
            assert_publication_did_every_sync(&events, flag, 2);
        }
    }
}
