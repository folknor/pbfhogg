//! Verify: inspect tags - `pbfhogg inspect tags` against the key counts
//! `osmium tags-count` gives over every kind and over ways alone.
//!
//! Both print one row per key, `count TAB key NEWLINE`, ordered by count
//! descending then key ascending, and DEVIATIONS.md (inspect tags) promises
//! the same counts and row order; they quote the key differently. osmium
//! wraps it in double quotes and doubles a quote inside, escaping nothing
//! else, so a tab or newline in a key is written literally; pbfhogg writes
//! a JSON string literal. The two reference reports are projected in one
//! pass over osmium's XML decoding, which preserves key bytes, including
//! bytes outside UTF-8. If the projection cannot be read, fall back to the
//! command that reads tags without metadata. The row lists must be equal in
//! order: the order is part of the promise, so it is never sorted away.
//!
//! libosmium keeps strings NUL-terminated, so a key with an embedded NUL is
//! outside what osmium can count faithfully, and DEVIATIONS.md excepts it.
//! A pbfhogg row whose key decodes with a NUL makes the comparison an error
//! naming the key, never a verdict either way.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::{Origin, ToolRun};

/// One report row: the count as printed (decimal digits, so no precision
/// is lost) and the decoded key.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Row {
    count: String,
    key: Vec<u8>,
}

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("inspect-tags")?;
    let mut projection = None;
    for (label, way) in [("all kinds", false), ("ways", true)] {
        harness.msg(&format!("=== verify inspect tags ({label}) ==="));
        let stem = if way { "way" } else { "all" };

        let ours_path = outdir.join(format!("pbfhogg-{stem}.txt"));
        let ended = harness.pbfhogg(
            PbfArgv::new("inspect")
                .arg("tags")
                .input(pbf)
                .arg_if(way, "--type")
                .arg_if(way, "way")
                .arg_if(direct_io, "--direct-io"),
            &ours_path,
        )?;
        harness.require_success(&ended, "pbfhogg inspect tags")?;

        if !way {
            projection = projected_reports(harness, pbf, &outdir)?;
        }
        let theirs_path = outdir.join(format!("osmium-{stem}.txt"));
        if let Some(reports) = &projection {
            write_osmium_report(&theirs_path, &reports[usize::from(way)])?;
        } else {
            let mut run = ToolRun::osmium("inspect-tags.1")
                .arg("tags-count")
                .input(pbf, Origin::Other);
            if way {
                run = run.args(&["-t", "way"]);
            }
            let ended = harness.reference(&run, &theirs_path)?;
            harness.require_success(&ended, "osmium tags-count")?;
        }

        let ours = report_rows("pbfhogg", &read(&ours_path)?, pbfhogg_field)?;
        let fallback;
        let theirs = if let Some(reports) = &projection {
            &reports[usize::from(way)]
        } else {
            fallback = report_rows("osmium", &read(&theirs_path)?, osmium_field)?;
            &fallback
        };
        if let Some(row) = ours.iter().find(|r| r.key.contains(&0)) {
            return Err(Error::Failed(format!(
                "pbfhogg reports key {:?} with an embedded NUL, which osmium cannot count \
                 faithfully (DEVIATIONS.md, inspect tags); the reports cannot be compared",
                String::from_utf8_lossy(&row.key)
            )));
        }
        let verdict = match first_difference(&ours, theirs) {
            None => {
                harness.msg(&format!(
                    "  inspect tags ({label}): PASS ({} rows)",
                    ours.len()
                ));
                Verdict::Pass
            }
            Some((i, a, b)) => {
                harness.msg(&format!(
                    "  inspect tags ({label}): FAIL at row {}: pbfhogg {}, osmium {}",
                    i + 1,
                    show(a),
                    show(b)
                ));
                Verdict::Fail
            }
        };
        harness.record(
            &format!("inspect tags ({label}) vs osmium tags-count"),
            verdict,
        );
    }
    Ok(Some(
        "every kind and ways alone: the key rows, counts and their order equal osmium \
         tags-count's, keys decoded from each tool's quoting; no key holds a NUL"
            .to_owned(),
    ))
}

fn projected_reports(
    harness: &mut Harness,
    pbf: &Path,
    outdir: &Path,
) -> Result<Option<[Vec<Row>; 2]>, Error> {
    const PROJECTION: &str = "inspect-tags counts 1";
    let xml = outdir.join("osmium-tags.osm");
    let run = ToolRun::osmium("inspect-tags-decode.1")
        .arg("cat")
        .input(pbf, Origin::Other)
        .args(&["-f", "osm,add_metadata=false", "-o"])
        .output(&xml)
        .arg("--overwrite")
        .uncached();
    harness.run.watch(pbf)?;
    let key = |harness: &mut Harness, reprobe| {
        Ok::<_, Error>(
            harness
                .summary_keys(std::slice::from_ref(&run), reprobe)?
                .and_then(|keys| keys.first().map(|key| key.derived(PROJECTION))),
        )
    };
    let planned = key(harness, true)?;
    if let Some(reports) = planned.as_ref().and_then(|key| harness.summary_of(key)) {
        harness.run.revalidate()?;
        harness.checkpoint()?;
        return Ok(Some(reports));
    }
    match harness.reference(&run, &outdir.join("osmium-tags.stdout")) {
        Ok(_) => {}
        // cat reads metadata that tags-count skips. A refusal to decode it
        // does not rule out a valid tags-count report.
        Err(Error::Tool(_)) => return Ok(None),
        Err(e) => return Err(e),
    }
    let reports = harness.timed("inspect tags projection", || {
        File::open(&xml).and_then(|file| xml_reports(BufReader::new(file)))
    });
    std::fs::remove_file(&xml)?;
    let Ok(Some(reports)) = reports else {
        return Ok(None);
    };
    if let Some(planned) = planned
        && key(harness, false)?.as_ref() == Some(&planned)
    {
        harness.publish_summary_of(&planned, &reports)?;
    }
    Ok(Some(reports))
}

/// Read the XML writer's one-line object and tag records as bytes. Osmium
/// escapes quotes, markup and ASCII whitespace in attributes; every other
/// byte of the C string is written verbatim, just as in tags-count.
fn xml_reports(mut xml: impl BufRead) -> std::io::Result<Option<[Vec<Row>; 2]>> {
    let mut counts = HashMap::<Vec<u8>, [Option<u32>; 2]>::new();
    let mut line = Vec::new();
    let mut key = Vec::new();
    let mut way = None;
    let mut complete = false;
    while xml.read_until(b'\n', &mut line)? != 0 {
        let record = line.trim_ascii();
        match record {
            r if r.starts_with(b"<way ") => way = (!r.ends_with(b"/>")).then_some(true),
            r if r.starts_with(b"<node ") || r.starts_with(b"<relation ") => {
                way = (!r.ends_with(b"/>")).then_some(false);
            }
            b"</way>" | b"</node>" | b"</relation>" => way = None,
            b"</osm>" => complete = true,
            r if r.starts_with(b"<tag ") => {
                let Some(way) = way else {
                    return Ok(None);
                };
                if !xml_key(r, &mut key) {
                    return Ok(None);
                }
                if let Some(count) = counts.get_mut(key.as_slice()) {
                    tally(count, way);
                } else {
                    tally(counts.entry(key.clone()).or_default(), way);
                }
            }
            r if r.starts_with(b"<?xml ")
                || r.starts_with(b"<osm ")
                || r.starts_with(b"<bounds ")
                || r.starts_with(b"<nd ")
                || r.starts_with(b"<member ") => {}
            _ => return Ok(None),
        }
        line.clear();
    }
    Ok(complete.then(|| [rows(&counts, 0), rows(&counts, 1)]))
}

fn xml_key(record: &[u8], key: &mut Vec<u8>) -> bool {
    let Some(attributes) = record.strip_prefix(b"<tag k=\"") else {
        return false;
    };
    let Some(end) = attributes.iter().position(|&b| b == b'"') else {
        return false;
    };
    if !attributes[end..].starts_with(b"\" v=\"") || !record.ends_with(b"\"/>") {
        return false;
    }
    key.clear();
    let mut left = &attributes[..end];
    while let Some(at) = left.iter().position(|&b| b == b'&') {
        key.extend_from_slice(&left[..at]);
        let entity = &left[at..];
        let Some((byte, rest)) = [
            (&b"&amp;"[..], b'&'),
            (b"&quot;", b'"'),
            (b"&apos;", b'\''),
            (b"&lt;", b'<'),
            (b"&gt;", b'>'),
            (b"&#x9;", b'\t'),
            (b"&#xA;", b'\n'),
            (b"&#xD;", b'\r'),
        ]
        .into_iter()
        .find_map(|(prefix, byte)| entity.strip_prefix(prefix).map(|rest| (byte, rest))) else {
            return false;
        };
        key.push(byte);
        left = rest;
    }
    key.extend_from_slice(left);
    true
}

fn tally(count: &mut [Option<u32>; 2], way: bool) {
    // tags-count uses uint32_t and prints a key even if its count wraps to 0.
    for count in &mut count[..=usize::from(way)] {
        *count = Some(count.unwrap_or(0).wrapping_add(1));
    }
}

fn rows(counts: &HashMap<Vec<u8>, [Option<u32>; 2]>, scope: usize) -> Vec<Row> {
    let mut rows: Vec<_> = counts
        .iter()
        .filter_map(|(key, count)| count[scope].map(|count| (count, key)))
        .collect();
    rows.sort_unstable_by_key(|&(count, key)| (Reverse(count), key));
    rows.into_iter()
        .map(|(count, key)| Row {
            count: count.to_string(),
            key: key.clone(),
        })
        .collect()
}

fn write_osmium_report(path: &Path, rows: &[Row]) -> Result<(), Error> {
    let mut out = BufWriter::new(File::create(path)?);
    for Row { count, key } in rows {
        write!(out, "{count}\t\"")?;
        for &byte in key {
            if byte == b'"' {
                out.write_all(b"\"")?;
            }
            out.write_all(&[byte])?;
        }
        out.write_all(b"\"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path).map_err(|e| Error::tool(format!("cannot read {}: {e}", path.display())))
}

fn show(row: Option<&Row>) -> String {
    row.map_or_else(
        || "no row".to_owned(),
        |r| format!("{} {:?}", r.count, String::from_utf8_lossy(&r.key)),
    )
}

/// The first row at which the lists differ, with each side's row there.
fn first_difference<'a>(
    a: &'a [Row],
    b: &'a [Row],
) -> Option<(usize, Option<&'a Row>, Option<&'a Row>)> {
    (0..a.len().max(b.len()))
        .map(|i| (i, a.get(i), b.get(i)))
        .find(|(_, x, y)| x != y)
}

/// A field parser: the decoded key starting at `pos` (its opening quote)
/// and the position after its closing quote.
type Field = fn(&[u8], usize) -> Result<(Vec<u8>, usize), String>;

/// Every byte of `text` as rows of decimal count, TAB, one quoted key,
/// NEWLINE. A blank line, a third field or trailing bytes fail.
fn report_rows(tool: &str, text: &[u8], field: Field) -> Result<Vec<Row>, Error> {
    let bad =
        |pos: usize, why: &str| Error::Failed(format!("{tool} tags report at byte {pos}: {why}"));
    let mut rows = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let digits = text[pos..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 || text.get(pos + digits) != Some(&b'\t') {
            return Err(bad(pos, "expected a count and a tab"));
        }
        let count = String::from_utf8_lossy(&text[pos..pos + digits]).into_owned();
        let (key, next) = field(text, pos + digits + 1).map_err(|why| bad(pos, &why))?;
        if text.get(next) != Some(&b'\n') {
            return Err(bad(pos, "expected the row to end after its key"));
        }
        rows.push(Row { count, key });
        pos = next + 1;
    }
    Ok(rows)
}

/// osmium's quoting: `"`, the key with each `"` doubled, `"`.
fn osmium_field(text: &[u8], pos: usize) -> Result<(Vec<u8>, usize), String> {
    if text.get(pos) != Some(&b'"') {
        return Err("key is not quoted".to_owned());
    }
    let mut key = Vec::new();
    let mut i = pos + 1;
    // loop-ends: the closing quote or the end of the text.
    loop {
        match text.get(i) {
            None => return Err("unterminated key".to_owned()),
            Some(b'"') if text.get(i + 1) == Some(&b'"') => {
                key.push(b'"');
                i += 2;
            }
            Some(b'"') => return Ok((key, i + 1)),
            Some(&b) => {
                key.push(b);
                i += 1;
            }
        }
    }
}

/// pbfhogg's quoting: one JSON string literal.
fn pbfhogg_field(text: &[u8], pos: usize) -> Result<(Vec<u8>, usize), String> {
    if text.get(pos) != Some(&b'"') {
        return Err("key is not quoted".to_owned());
    }
    let mut i = pos + 1;
    // loop-ends: the closing quote or the end of the text.
    loop {
        match text.get(i) {
            None => return Err("unterminated key".to_owned()),
            Some(b'\\') => i += 2,
            Some(b'"') => break,
            Some(_) => i += 1,
        }
    }
    let literal = std::str::from_utf8(&text[pos..=i]).map_err(|_| "key is not UTF-8".to_owned())?;
    let key: String =
        serde_json::from_str(literal).map_err(|e| format!("key is not a JSON string: {e}"))?;
    Ok((key.into_bytes(), i + 1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{first_difference, osmium_field, pbfhogg_field, report_rows, tally, xml_reports};

    const OURS: &[u8] = b"9\t\"highway\"\n4\t\"say \\\"hi\\\"\\\\\"\n2\t\"tab\\there\\nnl\"\n";
    const THEIRS: &[u8] = b"9\t\"highway\"\n4\t\"say \"\"hi\"\"\\\"\n2\t\"tab\there\nnl\"\n";

    #[test]
    fn one_decoding_matches_both_tags_count_reports() {
        let xml = &b"\
<?xml version='1.0' encoding='UTF-8'?>
<osm version=\"0.6\" generator=\"osmium\">
  <way id=\"1\">
    <nd ref=\"1\"/>
    <tag k=\"highway\" v=\"primary\"/>
    <tag k=\"highway\" v=\"secondary\"/>
    <tag k=\"a&amp;&quot;&apos;&lt;&gt;&#x9;&#xA;&#xD;\" v=\"&amp;\"/>
    <tag k=\"\xff\" v=\"\"/>
  </way>
  <node id=\"1\">
    <tag k=\"highway\" v=\"no\"/>
    <tag k=\"\" v=\"empty key\"/>
  </node>
  <way id=\"2\"/>
  <relation id=\"2\">
    <member type=\"way\" ref=\"1\" role=\"\"/>
    <tag k=\"name\" v=\"name\"/>
  </relation>
</osm>
"[..];
        let projected = xml_reports(xml).unwrap().unwrap();
        let all = b"3\t\"highway\"\n1\t\"\"\n1\t\"a&\"\"'<>\t\n\r\"\n1\t\"name\"\n1\t\"\xff\"\n";
        let ways = b"2\t\"highway\"\n1\t\"a&\"\"'<>\t\n\r\"\n1\t\"\xff\"\n";
        for (rows, report) in projected.iter().zip([&all[..], &ways[..]]) {
            assert_eq!(rows, &report_rows("osmium", report, osmium_field).unwrap());
        }
        let truncated = xml.strip_suffix(b"</osm>\n").unwrap();
        assert!(xml_reports(truncated).unwrap().is_none());
    }

    #[test]
    fn wrapped_counts_are_still_rows_in_their_own_scope() {
        let mut counts = [Some(u32::MAX), None];
        tally(&mut counts, false);
        assert_eq!(counts, [Some(0), None]);
        counts[1] = Some(u32::MAX);
        tally(&mut counts, true);
        assert_eq!(counts, [Some(1), Some(0)]);
    }

    #[test]
    fn rows_decode_equal_across_quoting() {
        let ours = report_rows("pbfhogg", OURS, pbfhogg_field).unwrap();
        let theirs = report_rows("osmium", THEIRS, osmium_field).unwrap();
        assert_eq!(ours.len(), 3);
        assert_eq!(ours[1].key, b"say \"hi\"\\");
        assert_eq!(ours[2].key, b"tab\there\nnl");
        assert!(first_difference(&ours, &theirs).is_none());
    }

    #[test]
    fn count_order_and_missing_rows_differ() {
        let rows = |t: &[u8]| report_rows("osmium", t, osmium_field).unwrap();
        let pairs: [(&[u8], &[u8]); 3] = [
            (b"9\t\"highway\"\n", b"8\t\"highway\"\n"),
            (b"1\t\"a\"\n1\t\"b\"\n", b"1\t\"b\"\n1\t\"a\"\n"),
            (b"1\t\"a\"\n1\t\"b\"\n", b"1\t\"a\"\n"),
        ];
        for (a, b) in pairs {
            assert!(first_difference(&rows(a), &rows(b)).is_some());
        }
    }

    #[test]
    fn malformed_rows_fail() {
        for (bad, why) in [
            (
                &b"1\t\"a\"\n\n"[..],
                "at byte 6: expected a count and a tab",
            ),
            (
                b"1\t\"a\"\t\"v\"\n",
                "at byte 0: expected the row to end after its key",
            ),
            (b"1\t\"a\n", "at byte 0: unterminated key"),
            (
                b"1\t\"a\"x\n",
                "at byte 0: expected the row to end after its key",
            ),
            (b"x\t\"a\"\n", "at byte 0: expected a count and a tab"),
            (
                b"1\t\"a\"",
                "at byte 0: expected the row to end after its key",
            ),
        ] {
            let osmium =
                crate::test_support::refused(report_rows("osmium", bad, osmium_field), why);
            assert!(osmium.to_string().starts_with("osmium tags report"));
            let pbfhogg =
                crate::test_support::refused(report_rows("pbfhogg", bad, pbfhogg_field), why);
            assert!(pbfhogg.to_string().starts_with("pbfhogg tags report"));
        }
    }

    #[test]
    fn a_nul_in_a_json_key_decodes() {
        let rows = report_rows("pbfhogg", b"1\t\"a\\u0000b\"\n", pbfhogg_field).unwrap();
        assert!(rows[0].key.contains(&0));
    }
}
