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
    pbfhogg::commands::merge_changes::merge_changes(&[input], out, false, Some(1), false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn simplify_verdict(input: &Path, out: &Path) -> Verdict {
    pbfhogg::commands::merge_changes::merge_changes(&[input], out, true, Some(1), false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn tags_filter_verdict(input: &Path, out: &Path) -> Verdict {
    tags_filter_osc(input, out, &["highway=primary".to_string()])
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
    pbfhogg::commands::merge_changes::merge_changes(&[&input], &merge_out, false, Some(1), false)
        .unwrap();
    let filter_out = dir.path().join("meta-out-filter.osc.gz");
    tags_filter_osc(&input, &filter_out, &["highway=primary".to_string()]).unwrap();
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
        )
        .expect("a re-emit path never interprets these, so it has nothing to refuse");
        let filter_out = dir.path().join("verbatim-out-filter.osc.gz");
        tags_filter_osc(&input, &filter_out, &["highway=primary".to_string()])
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
    pbfhogg::commands::merge_changes::merge_changes(&[&input], &merge_out, false, Some(1), false)
        .unwrap();
    let filter_out = dir.path().join("escape-out-filter.osc.gz");
    tags_filter_osc(&input, &filter_out, &["highway=primary".to_string()]).unwrap();

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
