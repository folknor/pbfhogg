//! One OSC document, three consumers, one answer.
//!
//! The change-stream hunt's headline finding was that the OSC XML state
//! machine had been copied three times - the compact overlay behind
//! `apply-changes`, the owned-element sink behind `merge-changes`, and a
//! third inside `tags-filter --input-kind osc` that the hunt only found
//! by accident - and that the copies had drifted apart. One of them
//! decoded gzip unconditionally, so a plain `.osc` was a "invalid gzip
//! header" error there and ordinary input everywhere else. One folded a
//! missing coordinate into 0/0. One accepted a relation member type its
//! own writer then refused. Every case was "one input, several answers".
//!
//! The copies are gone: all three now drive `osc::xml_parse::drive_osc`
//! behind an `OscSink`. But "they share a function" is a claim about the
//! code, and the property that actually matters is a claim about
//! behaviour, which only a test that feeds the *same bytes* to all three
//! can hold. Every case below is a divergence that was real; each is
//! asserted against all three consumers together, so a future sink that
//! re-implements any part of the framing has to disagree with two
//! others, loudly, here.
//!
//! Deliberately at the library layer rather than through `CliInvoker`:
//! the three sinks are three library entry points, and going through the
//! CLI would add three different output-guard and argument surfaces to a
//! test whose whole subject is the parser underneath them.

#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::GzEncoder;
use pbfhogg::tags_filter::osc::tags_filter_osc;
use tempfile::TempDir;

fn write_gz(path: &Path, xml: &str) {
    let file = File::create(path).unwrap();
    let mut enc = GzEncoder::new(file, Compression::fast());
    enc.write_all(xml.as_bytes()).unwrap();
    enc.finish().unwrap();
}

fn read_gz(path: &Path) -> String {
    use std::io::Read;
    let file = File::open(path).unwrap();
    let mut dec = flate2::read::MultiGzDecoder::new(file);
    let mut xml = String::new();
    dec.read_to_string(&mut xml).unwrap();
    xml
}

/// The outcome each consumer reached for one input: `Ok(())`, or the
/// error text.
type Verdict = Result<(), String>;

fn overlay_verdict(input: &Path) -> Verdict {
    pbfhogg::osc::parse_osc_file(input)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn merge_verdict(input: &Path, out: &Path) -> Verdict {
    pbfhogg::commands::merge_changes::merge_changes(
        &[input],
        out,
        false,
        Some(1),
        false,
        &crate::common::test_host(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn simplify_verdict(input: &Path, out: &Path) -> Verdict {
    pbfhogg::commands::merge_changes::merge_changes(
        &[input],
        out,
        true,
        Some(1),
        false,
        &crate::common::test_host(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn tags_filter_verdict(input: &Path, out: &Path) -> Verdict {
    tags_filter_osc(
        input,
        out,
        &["highway=primary".to_string()],
        &crate::common::test_host(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Run one input through every consumer. Returns `(name, verdict)` per
/// consumer so a failure message can name which one dissented.
fn all_verdicts(dir: &Path, input: &Path) -> Vec<(&'static str, Verdict)> {
    let out = |n: &str| -> PathBuf { dir.join(format!("out-{n}.osc.gz")) };
    vec![
        ("apply-changes overlay", overlay_verdict(input)),
        ("merge-changes streaming", merge_verdict(input, &out("m"))),
        ("merge-changes simplify", simplify_verdict(input, &out("s"))),
        ("tags-filter osc", tags_filter_verdict(input, &out("t"))),
    ]
}

/// Assert every consumer refused, and that each error text mentions
/// `needle` - so "they all failed" cannot be satisfied by three
/// unrelated failures.
fn assert_all_refuse(dir: &Path, input: &Path, needle: &str, case: &str) {
    for (name, verdict) in all_verdicts(dir, input) {
        let err = match verdict {
            Ok(()) => panic!("{case}: {name} accepted input the others refuse"),
            Err(e) => e,
        };
        assert!(
            err.contains(needle),
            "{case}: {name} refused for the wrong reason; wanted {needle:?}, got: {err}"
        );
    }
}

/// Assert every consumer accepted.
fn assert_all_accept(dir: &Path, input: &Path, case: &str) {
    for (name, verdict) in all_verdicts(dir, input) {
        if let Err(e) = verdict {
            panic!("{case}: {name} refused input the others accept: {e}");
        }
    }
}

/// The divergence that was real: `tags-filter --input-kind osc` wrapped
/// every input in a `MultiGzDecoder` whatever its bytes said, so a plain
/// uncompressed `.osc` - which the other two read fine - failed there
/// with a raw "invalid gzip header". Compression is sniffed from the
/// leading bytes now, and the file name is not consulted at all, so the
/// same document must read identically under either encoding and under
/// either extension.
#[test]
fn every_consumer_reads_plain_and_gzipped_osc() {
    let dir = TempDir::new().unwrap();
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"><tag k="highway" v="primary"/></node>
  </create>
</osmChange>"#;

    let gz = dir.path().join("in.osc.gz");
    write_gz(&gz, xml);
    assert_all_accept(dir.path(), &gz, "gzipped");

    let plain = dir.path().join("in.osc");
    std::fs::write(&plain, xml).unwrap();
    assert_all_accept(dir.path(), &plain, "plain");

    // The name lies in both directions: gzip bytes under a `.osc` name,
    // and plain bytes under a `.osc.gz` name. Sniffing means neither
    // matters.
    let lying_plain = dir.path().join("lies-plain.osc.gz");
    std::fs::write(&lying_plain, xml).unwrap();
    assert_all_accept(dir.path(), &lying_plain, "plain bytes named .gz");

    let lying_gz = dir.path().join("lies-gz.osc");
    write_gz(&lying_gz, xml);
    assert_all_accept(dir.path(), &lying_gz, "gzip bytes named .osc");
}

/// A complete gzip member with no closing `</osmChange>` is a truncated
/// download, and treating it as a whole document silently drops every
/// change after the cut while reporting success. Two of the three
/// copies had no truncation check at all.
#[test]
fn every_consumer_refuses_a_truncated_osc() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("cut.osc.gz");
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n",
    );
    assert_all_refuse(dir.path(), &input, "closing </osmChange>", "truncated");
}

/// A created or modified node without usable coordinates. Two copies
/// did `unwrap_or(0.0)` followed by a saturating `as i32`, which puts
/// the node at 0/0 - the Gulf of Guinea, a real place - and clamps
/// `lat="1e30"` to the pole. Absent, empty, unparsable, NaN,
/// unrepresentable and out-of-range are one rule now, in one function.
#[test]
fn every_consumer_refuses_an_unusable_coordinate() {
    let dir = TempDir::new().unwrap();
    for (case, attrs) in [
        ("absent", r#"id="1" version="1""#),
        ("empty", r#"id="1" lat="" lon="2.0" version="1""#),
        ("unparsable", r#"id="1" lat="abc" lon="2.0" version="1""#),
        ("nan", r#"id="1" lat="NaN" lon="2.0" version="1""#),
        (
            "unrepresentable",
            r#"id="1" lat="1e30" lon="2.0" version="1""#,
        ),
        ("out of range", r#"id="1" lat="91.0" lon="2.0" version="1""#),
    ] {
        let input = dir.path().join("coord.osc.gz");
        write_gz(
            &input,
            &format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
                 <node {attrs}/>\n  </create>\n</osmChange>"
            ),
        );
        assert_all_refuse(dir.path(), &input, "OSC node 1", case);
    }

    // The counterpart, so the rule is not "refuse every node": a delete
    // carries no coordinates by design, and the extremes of the valid
    // range are valid.
    let input = dir.path().join("ok.osc.gz");
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"-90.0\" lon=\"180.0\" version=\"1\"/>\n  </create>\n  \
         <delete>\n    <node id=\"2\" version=\"1\"/>\n  </delete>\n</osmChange>",
    );
    assert_all_accept(dir.path(), &input, "valid extremes and a bare delete");
}

/// A relation member whose `type` is not node, way or relation. The
/// overlay's writer refused to emit such a member, but two sinks
/// accepted it at parse time and one writer mapped it to "node" - which
/// produces a well-formed OSC pointing at a completely different object.
/// Refused at parse now, in the one shared `parse_member_type`, so no
/// sink can inherit half the rule.
#[test]
fn every_consumer_refuses_an_exotic_member_type() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("member.osc.gz");
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <relation id=\"1\" version=\"1\">\n      \
         <member type=\"changeset\" ref=\"5\" role=\"\"/>\n    </relation>\n  \
         </create>\n</osmChange>",
    );
    assert_all_refuse(dir.path(), &input, "member type", "exotic member type");

    // The counterpart: the three legal types all survive everywhere.
    let input = dir.path().join("member-ok.osc.gz");
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <relation id=\"1\" version=\"1\">\n      \
         <member type=\"node\" ref=\"5\" role=\"a\"/>\n      \
         <member type=\"way\" ref=\"6\" role=\"b\"/>\n      \
         <member type=\"relation\" ref=\"7\" role=\"c\"/>\n    </relation>\n  \
         </create>\n</osmChange>",
    );
    assert_all_accept(dir.path(), &input, "the three legal member types");
}

/// Metadata leniency is PRESENCE, not version-gating, in every sink: an
/// element carrying `timestamp` but no `version` still has metadata.
/// `merge-changes` used to return `None` for any element without a
/// `version`, discarding timestamp, changeset, uid, user and visible
/// along with it - so the same file kept its metadata under
/// `apply-changes` and lost it under `merge-changes`.
///
/// And a metadata attribute that is present but unparsable is an error
/// everywhere, never a silent zero: `version="abc"` folding into 0 is
/// what would make every later version look like an advance to the
/// version-precedence gate.
#[test]
fn every_consumer_agrees_on_metadata_leniency() {
    let dir = TempDir::new().unwrap();

    let input = dir.path().join("meta.osc.gz");
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"1.0\" lon=\"2.0\" timestamp=\"1970-01-02T00:00:00Z\" \
         changeset=\"7\" uid=\"9\" user=\"m\"><tag k=\"highway\" v=\"primary\"/>\
         </node>\n  </create>\n</osmChange>",
    );
    assert_all_accept(dir.path(), &input, "metadata without a version");

    // The metadata really does survive BOTH re-emit paths, and no sink
    // invents a version the producer never claimed. tags-filter is
    // asserted on its own emitted bytes, not just its exit verdict:
    // "metadata: None on every kept element" was a real bug there, and
    // a verdict-only parity check would have stayed green through it.
    let merge_out = dir.path().join("meta-out.osc");
    pbfhogg::commands::merge_changes::merge_changes(
        &[&input],
        &merge_out,
        false,
        Some(1),
        false,
        &crate::common::test_host(),
    )
    .unwrap();
    let filter_out = dir.path().join("meta-out-filter.osc.gz");
    tags_filter_osc(
        &input,
        &filter_out,
        &["highway=primary".to_string()],
        &crate::common::test_host(),
    )
    .unwrap();
    for (name, xml) in [
        (
            "merge-changes",
            std::fs::read_to_string(&merge_out).unwrap(),
        ),
        ("tags-filter", read_gz(&filter_out)),
    ] {
        assert!(
            xml.contains("timestamp=") && xml.contains("uid=") && xml.contains("user="),
            "{name}: metadata must survive an absent version; output was:\n{xml}"
        );
        assert!(
            !xml.contains("version=\"0\""),
            "{name}: no sink may invent version 0 for an element that claimed none; \
             output was:\n{xml}"
        );
    }

    // `version` is the one metadata attribute every sink interprets -
    // the version-precedence gate compares it - so an unparsable one is
    // an error in all of them. Folding `version="abc"` into 0, as the
    // old parsers did, is what would make every later version look like
    // an advance.
    for attrs in [
        r#"id="1" lat="1.0" lon="2.0" version="abc""#,
        r#"id="1" lat="1.0" lon="2.0" version="""#,
    ] {
        let input = dir.path().join("bad-version.osc.gz");
        write_gz(
            &input,
            &format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
                 <node {attrs}/>\n  </create>\n</osmChange>"
            ),
        );
        assert_all_refuse(dir.path(), &input, "unparsable value", attrs);
    }
}

/// The one metadata divergence that is deliberate, pinned here so it
/// stays deliberate.
///
/// `changeset`, `uid` and `timestamp` are refused when unparsable by the
/// overlay and accepted verbatim by the two re-emit sinks. That is not
/// drift: the overlay decodes them into fixed-width arena fields, so an
/// unparsable value has no representation there except a lie - the
/// absent-versus-zero collapse this hunt spent a round undoing.
/// `merge-changes` and `tags-filter` never interpret those three: the
/// bytes the producer wrote are copied to the bytes we write, so there
/// is nothing to misrepresent, and refusing would mean rejecting a file
/// we can process losslessly.
///
/// `version` is the exception in both directions because it is the only
/// metadata attribute the re-emit sinks *do* interpret - the
/// version-precedence gate compares it - which is why the test above
/// requires unanimity for that one and this one does not.
#[test]
fn only_the_overlay_interprets_changeset_uid_and_timestamp() {
    let dir = TempDir::new().unwrap();
    for attrs in [
        r#"id="1" lat="1.0" lon="2.0" version="1" changeset="not-a-number""#,
        r#"id="1" lat="1.0" lon="2.0" version="1" uid="x""#,
        r#"id="1" lat="1.0" lon="2.0" version="1" timestamp="yesterday""#,
    ] {
        let input = dir.path().join("verbatim.osc.gz");
        write_gz(
            &input,
            &format!(
                "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
                 <node {attrs}><tag k=\"highway\" v=\"primary\"/></node>\n  \
                 </create>\n</osmChange>"
            ),
        );
        let err = overlay_verdict(&input).expect_err("the overlay must refuse to store a lie");
        assert!(
            err.contains("unparsable value"),
            "overlay must name the unparsable value; got: {err}"
        );

        let merge_out = dir.path().join("verbatim-out.osc");
        pbfhogg::commands::merge_changes::merge_changes(
            &[&input],
            &merge_out,
            false,
            Some(1),
            false,
            &crate::common::test_host(),
        )
        .expect("a re-emit path never interprets these, so it has nothing to refuse");
        let filter_out = dir.path().join("verbatim-out-filter.osc.gz");
        tags_filter_osc(
            &input,
            &filter_out,
            &["highway=primary".to_string()],
            &crate::common::test_host(),
        )
        .expect("tags-filter is the other re-emit path and must agree");
        let (name, value) = attrs
            .rsplit_once('=')
            .map(|(head, v)| (head.rsplit(' ').next().unwrap_or(head), v.trim_matches('"')))
            .unwrap();
        for (consumer, xml) in [
            (
                "merge-changes",
                std::fs::read_to_string(&merge_out).unwrap(),
            ),
            ("tags-filter", read_gz(&filter_out)),
        ] {
            assert!(
                xml.contains(&format!("{name}=\"{value}\"")),
                "{consumer} must copy {name} through byte for byte; output was:\n{xml}"
            );
        }
    }
}

/// The re-emit sinks escape OSM user data identically, asserted on the
/// bytes each one emits. tags-filter used to push tag keys, tag values
/// and member roles as raw attributes, so a value containing a quote or
/// an ampersand produced malformed XML there and well-formed XML from
/// `merge-changes` for the same input - a payload divergence that a
/// verdict-only parity check cannot see, since both commands exit 0.
#[test]
fn re_emit_sinks_escape_user_data_identically() {
    let dir = TempDir::new().unwrap();
    let input = dir.path().join("escape.osc.gz");
    // Raw `&amp;` / `&quot;` / `&#10;` in the source decode to `&`, `"`
    // and a newline in the parsed value; the writers must re-escape all
    // three on the way out.
    write_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\">\
         <tag k=\"name\" v=\"a &quot;b&quot; &amp; c&#10;line2\"/>\
         <tag k=\"highway\" v=\"primary\"/></node>\n  </create>\n</osmChange>",
    );

    let merge_out = dir.path().join("escape-out.osc");
    pbfhogg::commands::merge_changes::merge_changes(
        &[&input],
        &merge_out,
        false,
        Some(1),
        false,
        &crate::common::test_host(),
    )
    .unwrap();
    let filter_out = dir.path().join("escape-out-filter.osc.gz");
    tags_filter_osc(
        &input,
        &filter_out,
        &["highway=primary".to_string()],
        &crate::common::test_host(),
    )
    .unwrap();

    for (consumer, xml) in [
        (
            "merge-changes",
            std::fs::read_to_string(&merge_out).unwrap(),
        ),
        ("tags-filter", read_gz(&filter_out)),
    ] {
        assert!(
            xml.contains("&quot;") && xml.contains("&amp;") && xml.contains("&#10;"),
            "{consumer} must escape quote, ampersand and newline in a tag value; \
             output was:\n{xml}"
        );
        // No raw quote may survive inside the attribute value: the value
        // written must parse back to the same bytes, which an unescaped
        // quote makes impossible.
        assert!(
            !xml.contains("v=\"a \"b\""),
            "{consumer} emitted an unescaped quote inside an attribute; output was:\n{xml}"
        );
    }
}

// ---------------------------------------------------------------------------
// Version precedence
// ---------------------------------------------------------------------------

/// Write one OSC file per body, in order, and return their paths.
fn write_sequence(dir: &Path, case: &str, bodies: &[&str]) -> Vec<PathBuf> {
    bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let path = dir.join(format!("{case}-{i:03}.osc.gz"));
            write_gz(
                &path,
                &format!(
                    "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  {body}\n</osmChange>"
                ),
            );
            path
        })
        .collect()
}

/// apply-changes' side: every file accumulated into one overlay, in
/// order, the way `load_all_diffs` and repeated `parse_osc_file_into`
/// calls build it.
fn overlay_sequence_verdict(inputs: &[PathBuf]) -> Verdict {
    let mut overlay = pbfhogg::osc::CompactDiffOverlay::new();
    for input in inputs {
        pbfhogg::osc::parse_osc_file_into(input, &mut overlay).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn simplify_sequence_verdict(inputs: &[PathBuf], out: &Path) -> Verdict {
    let refs: Vec<&Path> = inputs.iter().map(PathBuf::as_path).collect();
    pbfhogg::commands::merge_changes::merge_changes(
        &refs,
        out,
        true,
        Some(1),
        false,
        &crate::common::test_host(),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// The part of a version refusal that states the rule: everything after
/// the refusing consumer's name and the object, and before that
/// consumer's own opt-out advice. Anchored on the text rather than on a
/// separator, so a caller that wraps the error with file context does
/// not shift it.
fn refusal_core(err: &str) -> &str {
    let start = err.find("carries version").unwrap_or(0);
    let end = err
        .find("order and provenance")
        .map_or(err.len(), |i| i + "order and provenance".len());
    &err[start..end]
}

const UNVERSIONED_NODE: &str = r#"<modify><node id="1" lat="5.0" lon="5.0"/></modify>"#;

/// Sequences both gates must refuse: case name, one OSC body per file in
/// input order, and the floor the stale claim fails to advance past.
const REFUSED: &[(&str, &[&str], i32)] = &[
    (
        "bug017-delete-unversioned-stale",
        &[
            r#"<delete><node id="1" version="9"/></delete>"#,
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
    (
        "later-delete-raises-the-floor",
        &[
            r#"<modify><node id="1" lat="5.0" lon="5.0" version="5"/></modify>"#,
            r#"<delete><node id="1" version="9"/></delete>"#,
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
    (
        "unversioned-overwrite-of-a-live-claim",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
    (
        "unversioned-overwrite-above-an-old-delete",
        &[
            r#"<delete><node id="1" version="4"/></delete>"#,
            r#"<create><node id="1" lat="9.0" lon="9.0" version="9"/></create>"#,
            UNVERSIONED_NODE,
            r#"<delete><node id="1" version="8"/></delete>"#,
        ],
        9,
    ),
    (
        "unversioned-overwrite-of-a-live-way",
        &[
            r#"<modify><way id="7" version="4"><nd ref="1"/></way></modify>"#,
            r#"<modify><way id="7"><nd ref="1"/></way></modify>"#,
            r#"<modify><way id="7" version="3"><nd ref="1"/></way></modify>"#,
        ],
        4,
    ),
    (
        "unversioned-overwrite-of-a-live-relation",
        &[
            r#"<modify><relation id="7" version="4"><member type="node" ref="1" role=""/></relation></modify>"#,
            r#"<modify><relation id="7"><member type="node" ref="1" role=""/></relation></modify>"#,
            r#"<modify><relation id="7" version="3"><member type="node" ref="1" role=""/></relation></modify>"#,
        ],
        4,
    ),
    (
        "two-unversioned-overwrites",
        &[
            r#"<create><node id="1" lat="9.0" lon="9.0" version="9"/></create>"#,
            UNVERSIONED_NODE,
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
    (
        "later-modify-raises-the-floor",
        &[
            r#"<modify><node id="1" lat="5.0" lon="5.0" version="5"/></modify>"#,
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
    (
        "bug017-in-one-file",
        &[concat!(
            r#"<delete><node id="1" version="9"/></delete>"#,
            r#"<modify><node id="1" lat="5.0" lon="5.0"/></modify>"#,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        )],
        9,
    ),
    (
        "bug017-unversioned-delete-between",
        &[
            r#"<delete><node id="1" version="9"/></delete>"#,
            r#"<delete><node id="1"/></delete>"#,
            r#"<delete><node id="1" version="8"/></delete>"#,
        ],
        9,
    ),
    (
        "bug017-way",
        &[
            r#"<delete><way id="7" version="4"/></delete>"#,
            r#"<modify><way id="7"><nd ref="1"/></way></modify>"#,
            r#"<modify><way id="7" version="3"><nd ref="1"/></way></modify>"#,
        ],
        4,
    ),
    (
        "bug017-relation",
        &[
            r#"<delete><relation id="7" version="4"/></delete>"#,
            r#"<modify><relation id="7"><member type="node" ref="1" role=""/></relation></modify>"#,
            r#"<modify><relation id="7" version="3"><member type="node" ref="1" role=""/></relation></modify>"#,
        ],
        4,
    ),
    (
        "lower-after-equal-claims",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            r#"<delete><node id="1" version="9"/></delete>"#,
            r#"<create><node id="1" lat="9.0" lon="9.0" version="9"/></create>"#,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="8"/></modify>"#,
        ],
        9,
    ),
];

/// apply-changes and `merge-changes --simplify` both keep the last change
/// per object by position, so both gate version precedence, and they must
/// gate it by the same rule: an incoming version claim may not be lower
/// than the highest version ever claimed for the object, deletes
/// included, and a change that claims no version neither raises nor
/// resets that floor.
///
/// The divergence that was real: `--simplify` compared only against the
/// previous change, so `delete v9`, an unversioned `modify`, then
/// `modify v8` was refused by apply-changes (whose versioned tombstone
/// still held 9) and accepted by merge-changes (whose previous change
/// claimed nothing). Each case is fed, as the same files in the same
/// order, through both, and the refusals must agree on the explanation,
/// not just on failing.
///
/// The mirror divergence was real too, in the overlay: an unversioned
/// create or modify overwrote the live record and took its version with
/// it, so `modify v9`, an unversioned `modify`, then `modify v8` was
/// accepted there, and refused by merge-changes once it kept the highest
/// claim.
#[test]
fn both_version_gates_refuse_a_claim_below_the_highest_ever_made() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("simplify-out.osc.gz");
    for &(case, bodies, floor) in REFUSED {
        let inputs = write_sequence(dir.path(), case, bodies);
        let Err(overlay) = overlay_sequence_verdict(&inputs) else {
            panic!("{case}: apply-changes overlay accepted a stale claim");
        };
        let Err(simplify) = simplify_sequence_verdict(&inputs, &out) else {
            panic!("{case}: merge-changes --simplify accepted a stale claim");
        };
        let needle = format!("is lower than version {floor}");
        for (name, err) in [
            ("overlay", overlay.as_str()),
            ("simplify", simplify.as_str()),
        ] {
            assert!(
                err.contains(&needle),
                "{case}: {name} refused for the wrong reason; wanted {needle:?}, got: {err}"
            );
        }
        assert_eq!(
            refusal_core(overlay.as_str()),
            refusal_core(simplify.as_str()),
            "{case}: the two refusals must state the rule identically"
        );
    }
}

/// The counterparts, so the agreement above is not satisfied by both
/// gates refusing everything after an unversioned change.
const ACCEPTED: &[(&str, &[&str])] = &[
    (
        "advance-past-the-delete-floor",
        &[
            r#"<delete><node id="1" version="9"/></delete>"#,
            UNVERSIONED_NODE,
            r#"<create><node id="1" lat="8.0" lon="8.0" version="10"/></create>"#,
        ],
    ),
    (
        "advance-past-an-overwritten-live-claim",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="10"/></modify>"#,
        ],
    ),
    (
        "in-sequence",
        &[
            r#"<create><node id="1" lat="1.0" lon="1.0" version="1"/></create>"#,
            r#"<modify><node id="1" lat="2.0" lon="2.0" version="2"/></modify>"#,
            r#"<delete><node id="1" version="3"/></delete>"#,
        ],
    ),
    (
        "unversioned-only",
        &[
            r#"<create><node id="1" lat="1.0" lon="1.0"/></create>"#,
            UNVERSIONED_NODE,
            r#"<delete><node id="1"/></delete>"#,
        ],
    ),
    (
        "first-claim-after-unversioned",
        &[
            UNVERSIONED_NODE,
            r#"<modify><node id="1" lat="2.0" lon="2.0" version="0"/></modify>"#,
        ],
    ),
    // Equal claims: admitted, the later change wins by position.
    // Overlapping replication diffs repeat boundary changes verbatim.
    (
        "repeated-claim",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
        ],
    ),
    // The germany seq 4705/4706 shape: a regional extract deletes an
    // object leaving the region at the version it already had.
    (
        "regional-delete-at-the-held-version",
        &[
            r#"<modify><node id="1" lat="50.6575433" lon="9.511884" version="12" timestamp="2026-02-24T21:21:17Z"/></modify>"#,
            r#"<delete><node id="1" lat="50.6575433" lon="9.511884" version="12" timestamp="2026-02-24T21:21:17Z"/></delete>"#,
        ],
    ),
    // ... and `diff --format osc` without `--increment-version` emits that
    // delete carrying the version alone.
    (
        "regional-delete-version-only",
        &[
            r#"<modify><way id="7" version="4" timestamp="2026-02-24T21:21:17Z" uid="3" user="m"><nd ref="1"/></way></modify>"#,
            r#"<delete><way id="7" version="4"/></delete>"#,
        ],
    ),
    (
        "repeated-delete-and-unchanged-reentry",
        &[
            r#"<delete><relation id="7" version="4"/></delete>"#,
            r#"<delete><relation id="7" version="4"/></delete>"#,
            r#"<create><relation id="7" version="4"><member type="node" ref="1" role=""/></relation></create>"#,
        ],
    ),
    (
        "equal-claim-different-content",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="9"/></modify>"#,
        ],
    ),
    (
        "equal-claim-after-delete-floor",
        &[
            r#"<delete><node id="1" version="9"/></delete>"#,
            UNVERSIONED_NODE,
            r#"<create><node id="1" lat="8.0" lon="8.0" version="9"/></create>"#,
        ],
    ),
];

#[test]
fn both_version_gates_admit_a_claim_above_the_highest_ever_made() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("simplify-out.osc.gz");
    for &(case, bodies) in ACCEPTED {
        let inputs = write_sequence(dir.path(), case, bodies);
        if let Err(e) = overlay_sequence_verdict(&inputs) {
            panic!("{case}: apply-changes overlay refused an advancing sequence: {e}");
        }
        if let Err(e) = simplify_sequence_verdict(&inputs, &out) {
            panic!("{case}: merge-changes --simplify refused an advancing sequence: {e}");
        }
    }
}

/// Admitting an equal claim is not enough: both consumers must also keep
/// the same winner, the later change by position. Checked on what each
/// one holds afterwards - the overlay's live record or delete, and the
/// change `--simplify` emits.
#[test]
fn both_version_gates_let_the_later_equal_claim_win() {
    let dir = TempDir::new().unwrap();
    let out = dir.path().join("simplify-out.osc.gz");

    let inputs = write_sequence(
        dir.path(),
        "equal-content-wins",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="9"/></modify>"#,
            r#"<modify><node id="1" lat="8.0" lon="8.0" version="9"/></modify>"#,
        ],
    );
    let mut overlay = pbfhogg::osc::CompactDiffOverlay::new();
    for input in &inputs {
        pbfhogg::osc::parse_osc_file_into(input, &mut overlay).unwrap();
    }
    assert_eq!(overlay.get_node(1).unwrap().decimicro_lat(), 80_000_000);
    simplify_sequence_verdict(&inputs, &out).unwrap();
    let xml = read_gz(&out);
    assert!(
        xml.contains(r#"lat="8""#) && !xml.contains(r#"lat="9""#),
        "simplify must keep the later equal claim; output was:\n{xml}"
    );

    let inputs = write_sequence(
        dir.path(),
        "equal-delete-wins",
        &[
            r#"<modify><node id="1" lat="9.0" lon="9.0" version="12"/></modify>"#,
            r#"<delete><node id="1" version="12"/></delete>"#,
        ],
    );
    let mut overlay = pbfhogg::osc::CompactDiffOverlay::new();
    for input in &inputs {
        pbfhogg::osc::parse_osc_file_into(input, &mut overlay).unwrap();
    }
    assert!(!overlay.has_node(1));
    assert!(overlay.is_deleted(pbfhogg::ElementKind::Node, 1));
    simplify_sequence_verdict(&inputs, &out).unwrap();
    let xml = read_gz(&out);
    assert!(
        xml.contains("<delete>") && !xml.contains("<modify>"),
        "simplify must keep the later equal-version delete; output was:\n{xml}"
    );
}
