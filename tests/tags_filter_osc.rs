//! tags-filter-osc correctness tests.

use std::fs::File;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use pbfhogg::tags_filter::osc::tags_filter_osc;
use tempfile::TempDir;

fn exprs(strs: &[&str]) -> Vec<String> {
    strs.iter().map(ToString::to_string).collect()
}

fn write_osc_gz(path: &std::path::Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn read_osc_gz(path: &std::path::Path) -> String {
    let file = File::open(path).expect("open osc file");
    let mut dec = GzDecoder::new(file);
    let mut xml = String::new();
    dec.read_to_string(&mut xml).expect("read osc");
    xml
}

#[test]
fn create_and_modify_are_filtered_but_deletes_are_preserved() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("in.osc.gz");
    let output = dir.path().join("out.osc.gz");

    write_osc_gz(
        &input,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0">
      <tag k="highway" v="primary"/>
    </node>
    <node id="2" lat="3.0" lon="4.0">
      <tag k="name" v="foo"/>
    </node>
  </create>
  <modify>
    <way id="10">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="primary"/>
    </way>
    <way id="11">
      <tag k="name" v="bar"/>
    </way>
  </modify>
  <delete>
    <node id="99"/>
    <way id="88"/>
    <relation id="77"/>
  </delete>
</osmChange>"#,
    );

    let stats = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["highway=primary"]),
        &crate::common::test_host(),
    )
    .expect("filter");
    let xml = read_osc_gz(&output);

    assert_eq!(stats.creates_in, 2);
    assert_eq!(stats.creates_out, 1);
    assert_eq!(stats.modifies_in, 2);
    assert_eq!(stats.modifies_out, 1);
    assert_eq!(stats.deletes_in, 3);
    assert_eq!(stats.deletes_out, 3);

    assert!(xml.contains(r#"<create>"#));
    assert!(xml.contains(r#"<node id="1""#));
    assert!(!xml.contains(r#"<node id="2""#));

    assert!(xml.contains(r#"<modify>"#));
    assert!(xml.contains(r#"<way id="10""#));
    assert!(!xml.contains(r#"<way id="11""#));

    assert!(xml.contains(r#"<delete>"#));
    assert!(xml.contains(r#"<node id="99"/>"#));
    assert!(xml.contains(r#"<way id="88"/>"#));
    assert!(xml.contains(r#"<relation id="77"/>"#));
}

#[test]
fn type_prefix_applies_to_create_modify_only() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("in.osc.gz");
    let output = dir.path().join("out.osc.gz");

    write_osc_gz(
        &input,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="1.0">
      <tag k="amenity" v="bench"/>
    </node>
    <way id="10">
      <nd ref="1"/>
      <tag k="amenity" v="bench"/>
    </way>
  </create>
  <modify>
    <relation id="100">
      <member type="way" ref="10" role="outer"/>
      <tag k="amenity" v="bench"/>
    </relation>
  </modify>
  <delete>
    <node id="42"/>
  </delete>
</osmChange>"#,
    );

    let stats = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["w/amenity=bench"]),
        &crate::common::test_host(),
    )
    .expect("filter");
    let xml = read_osc_gz(&output);

    assert_eq!(stats.creates_out, 1);
    assert_eq!(stats.modifies_out, 0);
    assert_eq!(stats.deletes_out, 1);

    assert!(xml.contains(r#"<way id="10""#));
    assert!(!xml.contains(r#"<node id="1""#));
    assert!(!xml.contains(r#"<relation id="100""#));
    assert!(xml.contains(r#"<node id="42"/>"#));
}

#[test]
fn multiple_expressions_use_or_semantics() {
    let dir = TempDir::new().expect("tempdir");
    let input = dir.path().join("in.osc.gz");
    let output = dir.path().join("out.osc.gz");

    write_osc_gz(
        &input,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="1.0">
      <tag k="amenity" v="bench"/>
    </node>
    <node id="2" lat="2.0" lon="2.0">
      <tag k="shop" v="bakery"/>
    </node>
    <node id="3" lat="3.0" lon="3.0">
      <tag k="name" v="x"/>
    </node>
  </create>
</osmChange>"#,
    );

    let stats = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["amenity", "shop"]),
        &crate::common::test_host(),
    )
    .expect("filter");
    let xml = read_osc_gz(&output);

    assert_eq!(stats.creates_in, 3);
    assert_eq!(stats.creates_out, 2);
    assert!(xml.contains(r#"<node id="1""#));
    assert!(xml.contains(r#"<node id="2""#));
    assert!(!xml.contains(r#"<node id="3""#));
}

// ---------------------------------------------------------------------------
// The third OSC parser
//
// `tags-filter --input-kind osc` carried its own hand-rolled copy of the
// OSC state machine, which the change-stream bug hunt did not find. It
// had drifted exactly the way the other two had. These pin the shared
// framing it now inherits.
// ---------------------------------------------------------------------------

/// An uncompressed `.osc` is accepted. This parser wrapped every input
/// in `MultiGzDecoder` unconditionally, so a plain `.osc` failed with a
/// raw "invalid gzip header".
#[test]
fn plain_osc_input_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osc");
    let output = dir.path().join("out.osc.gz");
    std::fs::write(
        &input,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"><tag k="amenity" v="cafe"/></node>
  </create>
</osmChange>"#,
    )
    .expect("write plain osc");

    let stats = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["amenity"]),
        &crate::common::test_host(),
    )
    .expect("filter");
    assert_eq!(stats.creates_out, 1);
    assert!(read_osc_gz(&output).contains(r#"<node id="1""#));
}

/// A truncated OSC is refused rather than filtered as though it were
/// whole - which would drop every change after the cut and report
/// success.
#[test]
fn truncated_osc_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("cut.osc.gz");
    let output = dir.path().join("out.osc.gz");
    write_osc_gz(
        &input,
        "<?xml version=\"1.0\"?>\n<osmChange version=\"0.6\">\n  <create>\n    \
         <node id=\"1\" lat=\"1.0\" lon=\"2.0\" version=\"1\"/>\n  </create>\n",
    );
    let err = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["amenity"]),
        &crate::common::test_host(),
    )
    .expect_err("a truncated OSC must not filter as a complete one");
    assert!(
        err.to_string().contains("closing </osmChange>"),
        "error must name the missing closing tag; got: {err}"
    );
}

/// A created node without usable coordinates is refused. This parser
/// used `attr_f64(..).unwrap_or(0.0)` and a saturating `as i32`, so such
/// a node was re-emitted at 0/0 - a real place in the Gulf of Guinea.
#[test]
fn node_without_usable_coords_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.osc.gz");
    let output = dir.path().join("out.osc.gz");
    write_osc_gz(
        &input,
        r#"<?xml version="1.0"?>
<osmChange version="0.6">
  <create>
    <node id="1" version="1"><tag k="amenity" v="cafe"/></node>
  </create>
</osmChange>"#,
    );
    let err = tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["amenity"]),
        &crate::common::test_host(),
    )
    .expect_err("a node without coordinates must not be filtered to 0/0");
    let text = err.to_string();
    assert!(
        text.contains("OSC node 1") && text.contains("lat"),
        "error must name the node and the attribute; got: {text}"
    );
}

#[test]
fn legacy_output_is_gzip_without_suffix_and_metadata_is_normalized() {
    let dir = tempfile::tempdir_in(".").expect("directory");
    let input = dir.path().join("in.osc");
    let output = dir.path().join("out.unknown");
    std::fs::write(&input, b"<osmChange version=\"0.6\"><create><node id=\"7\" lat=\"1\" lon=\"2\" timestamp=\"yesterday\" changeset=\"no\" uid=\"&#48;\" visible=\"False\"><tag k=\"name\" v=\"test\"/></node></create></osmChange>").expect("XML");
    tags_filter_osc(
        &input,
        crate::common::out(&output),
        &exprs(&["name"]),
        &crate::common::test_host(),
    )
    .expect("filter");
    let xml = read_osc_gz(&output);
    for attribute in [
        "timestamp=\"yesterday\"",
        "changeset=\"no\"",
        "uid=\"0\"",
        "visible=\"False\"",
    ] {
        assert!(xml.contains(attribute), "{xml}");
    }
}
