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

fn read_osc_gz(path: &std::path::Path) -> String {
    let file = File::open(path).expect("open osc file");
    let mut dec = GzDecoder::new(file);
    let mut xml = String::new();
    dec.read_to_string(&mut xml).expect("read osc");
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
    <node id="1" lat="1.0" lon="2.0" version="1">
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
    <node id="1" lat="1.1" lon="2.1" version="2">
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
