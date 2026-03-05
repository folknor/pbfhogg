//! merge-changes correctness tests.

use std::fs::File;
use std::io::{Read, Write};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use pbfhogg::merge_changes::merge_changes;
use tempfile::TempDir;

fn write_osc_gz(path: &std::path::Path, xml: &str) {
    let file = File::create(path).expect("create osc file");
    let mut enc = GzEncoder::new(file, Compression::fast());
    enc.write_all(xml.as_bytes()).expect("write xml");
    enc.finish().expect("finish gz");
}

fn write_osc_plain(path: &std::path::Path, xml: &str) {
    let mut file = File::create(path).expect("create osc file");
    file.write_all(xml.as_bytes()).expect("write xml");
}

fn read_osc_gz(path: &std::path::Path) -> String {
    let file = File::open(path).expect("open osc file");
    let mut dec = GzDecoder::new(file);
    let mut xml = String::new();
    dec.read_to_string(&mut xml).expect("read osc");
    xml
}

fn read_osc_plain(path: &std::path::Path) -> String {
    let mut file = File::open(path).expect("open osc file");
    let mut xml = String::new();
    file.read_to_string(&mut xml).expect("read osc");
    xml
}

#[test]
fn merge_changes_keeps_full_stream_by_default() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let in2 = dir.path().join("002.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1" timestamp="2025-01-15T10:30:00Z" changeset="12345" uid="100" user="testuser">
      <tag k="name" v="first"/>
    </node>
  </create>
</osmChange>"#,
    );

    write_osc_gz(
        &in2,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2" timestamp="2025-01-16T12:00:00Z" changeset="12346" uid="200" user="otheruser">
      <tag k="name" v="second"/>
    </node>
  </modify>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1, &in2], &out, false).expect("merge-changes");
    assert_eq!(stats.files, 2);
    assert_eq!(stats.changes_in, 2);
    assert_eq!(stats.changes_out, 2);
    assert!(!stats.simplified);

    let xml = read_osc_gz(&out);
    assert!(xml.contains("<create>"));
    assert!(xml.contains("<modify>"));
    assert!(xml.contains(r#"version="1""#));
    assert!(xml.contains(r#"version="2""#));
    assert!(xml.contains(r#"v="first""#));
    assert!(xml.contains(r#"v="second""#));
    // Metadata attributes are preserved
    assert!(xml.contains(r#"timestamp="2025-01-15T10:30:00Z""#));
    assert!(xml.contains(r#"changeset="12345""#));
    assert!(xml.contains(r#"uid="100""#));
    assert!(xml.contains(r#"user="testuser""#));
    assert!(xml.contains(r#"timestamp="2025-01-16T12:00:00Z""#));
    assert!(xml.contains(r#"user="otheruser""#));
}

#[test]
fn simplify_keeps_only_last_change_per_object() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let in2 = dir.path().join("002.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
    <way id="10" version="1">
      <nd ref="1"/>
    </way>
  </create>
</osmChange>"#,
    );

    write_osc_gz(
        &in2,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <delete>
    <node id="1" version="2"/>
  </delete>
  <modify>
    <way id="10" version="2">
      <nd ref="1"/>
      <tag k="highway" v="residential"/>
    </way>
  </modify>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1, &in2], &out, true).expect("merge-changes simplify");
    assert_eq!(stats.files, 2);
    assert_eq!(stats.changes_in, 4);
    assert_eq!(stats.changes_out, 2);
    assert!(stats.simplified);

    let xml = read_osc_gz(&out);
    assert!(xml.contains("<delete>"));
    assert!(xml.contains(r#"<node id="1" version="2"/>"#));
    assert!(xml.contains("<modify>"));
    assert!(xml.contains(r#"<way id="10" version="2">"#));

    assert!(!xml.contains(r#"<node id="1" lat="1""#));
    assert!(!xml.contains(r#"<way id="10" version="1""#));
}

#[test]
fn plain_osc_input_supported() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc");
    let out = dir.path().join("out.osc");

    write_osc_plain(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
  </create>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1], &out, false).expect("merge plain osc");
    assert_eq!(stats.changes_out, 1);

    let xml = read_osc_plain(&out);
    assert!(xml.contains(r#"id="1""#));
}

#[test]
fn relation_roundtrip() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <relation id="100" version="1">
      <member type="way" ref="10" role="outer"/>
      <member type="way" ref="11" role="inner"/>
      <tag k="type" v="multipolygon"/>
    </relation>
  </create>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1], &out, false).expect("merge relation");
    assert_eq!(stats.changes_out, 1);

    let xml = read_osc_gz(&out);
    assert!(xml.contains(r#"<relation id="100" version="1">"#));
    assert!(xml.contains(r#"type="way""#));
    assert!(xml.contains(r#"ref="10""#));
    assert!(xml.contains(r#"role="outer""#));
    assert!(xml.contains(r#"role="inner""#));
    assert!(xml.contains(r#"v="multipolygon""#));
}

#[test]
fn simplify_multiple_same_type_different_ids() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1"/>
    <node id="2" lat="3.0" lon="4.0" version="1"/>
  </create>
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2"/>
  </modify>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1], &out, true).expect("simplify same type");
    assert_eq!(stats.changes_in, 3);
    assert_eq!(stats.changes_out, 2);

    let xml = read_osc_gz(&out);
    // node 1 simplified to modify (last action)
    assert!(xml.contains(r#"<node id="1" version="2""#));
    // node 2 stays as create
    assert!(xml.contains(r#"<node id="2" version="1""#));
}

#[test]
fn simplify_preserves_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
  <create>
    <node id="1" lat="1.0" lon="2.0" version="1" timestamp="2025-01-01T00:00:00Z" uid="42" user="alice"/>
  </create>
  <modify>
    <node id="1" lat="1.1" lon="2.1" version="2" timestamp="2025-06-15T12:00:00Z" uid="99" user="bob"/>
  </modify>
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1], &out, true).expect("simplify metadata");
    assert_eq!(stats.changes_out, 1);

    let xml = read_osc_gz(&out);
    // Should have the last version's metadata
    assert!(xml.contains(r#"version="2""#));
    assert!(xml.contains(r#"timestamp="2025-06-15T12:00:00Z""#));
    assert!(xml.contains(r#"user="bob""#));
    // Old metadata should be gone
    assert!(!xml.contains(r#"user="alice""#));
}

#[test]
fn empty_osc_produces_empty_output() {
    let dir = TempDir::new().expect("tempdir");
    let in1 = dir.path().join("001.osc.gz");
    let out = dir.path().join("out.osc.gz");

    write_osc_gz(
        &in1,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<osmChange version="0.6">
</osmChange>"#,
    );

    let stats = merge_changes(&[&in1], &out, false).expect("empty osc");
    assert_eq!(stats.changes_out, 0);

    let xml = read_osc_gz(&out);
    assert!(xml.contains("osmChange"));
    assert!(!xml.contains("<create>"));
    assert!(!xml.contains("<modify>"));
    assert!(!xml.contains("<delete>"));
}
