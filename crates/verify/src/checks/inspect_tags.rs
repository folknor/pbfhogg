//! Verify: inspect tags - `pbfhogg inspect tags` against `osmium tags-count`,
//! over every kind and over ways alone.
//!
//! Both print one row per key, `count TAB key NEWLINE`, ordered by count
//! descending then key ascending, and DEVIATIONS.md (inspect tags) promises
//! the same counts and row order; they quote the key differently. osmium
//! wraps it in double quotes and doubles a quote inside, escaping nothing
//! else, so a tab or newline in a key is written literally; pbfhogg writes
//! a JSON string literal. Each report is parsed whole, every byte
//! consumed, each key decoded by its tool's rule, and the two row lists
//! must be equal in order: the order is part of the promise, so it is never
//! sorted away.
//!
//! libosmium keeps strings NUL-terminated, so a key with an embedded NUL is
//! outside what osmium can count faithfully, and DEVIATIONS.md excepts it.
//! A pbfhogg row whose key decodes with a NUL makes the comparison an error
//! naming the key, never a verdict either way.

use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::reference::ToolRun;

/// One report row: the count as printed (decimal digits, so no precision
/// is lost) and the decoded key.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    count: String,
    key: Vec<u8>,
}

pub fn run(harness: &mut Harness, pbf: &Path, direct_io: bool) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("inspect-tags")?;
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

        let theirs_path = outdir.join(format!("osmium-{stem}.txt"));
        let mut run = ToolRun::osmium("inspect-tags.1")
            .arg("tags-count")
            .input(pbf);
        if way {
            run = run.args(&["-t", "way"]);
        }
        let ended = harness.reference(&run, &theirs_path)?;
        harness.require_success(&ended, "osmium tags-count")?;

        let ours = report_rows("pbfhogg", &read(&ours_path)?, pbfhogg_field)?;
        let theirs = report_rows("osmium", &read(&theirs_path)?, osmium_field)?;
        if let Some(row) = ours.iter().find(|r| r.key.contains(&0)) {
            return Err(Error::Failed(format!(
                "pbfhogg reports key {:?} with an embedded NUL, which osmium cannot count \
                 faithfully (DEVIATIONS.md, inspect tags); the reports cannot be compared",
                String::from_utf8_lossy(&row.key)
            )));
        }
        let verdict = match first_difference(&ours, &theirs) {
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
    use super::{first_difference, osmium_field, pbfhogg_field, report_rows};

    const OURS: &[u8] = b"9\t\"highway\"\n4\t\"say \\\"hi\\\"\\\\\"\n2\t\"tab\\there\\nnl\"\n";
    const THEIRS: &[u8] = b"9\t\"highway\"\n4\t\"say \"\"hi\"\"\\\"\n2\t\"tab\there\nnl\"\n";

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
        for bad in [
            &b"1\t\"a\"\n\n"[..],
            b"1\t\"a\"\t\"v\"\n",
            b"1\t\"a\n",
            b"1\t\"a\"x\n",
            b"x\t\"a\"\n",
            b"1\t\"a\"",
        ] {
            assert!(report_rows("osmium", bad, osmium_field).is_err(), "{bad:?}");
            assert!(
                report_rows("pbfhogg", bad, pbfhogg_field).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_nul_in_a_json_key_decodes() {
        let rows = report_rows("pbfhogg", b"1\t\"a\\u0000b\"\n", pbfhogg_field).unwrap();
        assert!(rows[0].key.contains(&0));
    }
}
