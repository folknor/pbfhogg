//! Bounded-memory readers over a step's stdout file.
//!
//! A listing step (a diff of two PBFs, an osmium diff) writes its stdout to
//! a file in the check's directory, because its size scales with the data.
//! Everything here reads such a file through fixed buffers, whatever its
//! length and however long its lines.
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use crate::error::Error;

fn open(path: &Path) -> Result<BufReader<File>, Error> {
    File::open(path)
        .map(BufReader::new)
        .map_err(|e| Error::tool(format!("cannot open {}: {e}", path.display())))
}

fn read_error(path: &Path, e: &io::Error) -> Error {
    Error::tool(format!("cannot read {}: {e}", path.display()))
}

/// Read a small textual tool report. Invalid text is a comparison failure;
/// inability to read the verifier's sink is an infrastructure error.
pub(crate) fn read_report(path: &Path) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            Error::Failed(format!("report {} is not UTF-8: {e}", path.display()))
        } else {
            read_error(path, &e)
        }
    })
}

/// Call `f` once per line with at most its first `head` bytes (newline
/// excluded), assembling heads across buffer refills.
pub(crate) fn for_each_line_head(
    path: &Path,
    head: usize,
    f: impl FnMut(&[u8]),
) -> Result<(), Error> {
    line_heads(open(path)?, head, f).map_err(|e| read_error(path, &e))
}

fn line_heads(mut reader: impl BufRead, head: usize, mut f: impl FnMut(&[u8])) -> io::Result<()> {
    let mut current: Vec<u8> = Vec::with_capacity(head);
    let mut in_line = false;
    // loop-ends: end of input.
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if in_line {
                f(&current);
            }
            return Ok(());
        }
        let len = buf.len();
        let mut start = 0;
        while start < len {
            in_line = true;
            let rest = &buf[start..];
            let (segment, ended) = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            let room = head.saturating_sub(current.len());
            current.extend_from_slice(&segment[..segment.len().min(room)]);
            if ended {
                f(&current);
                current.clear();
                in_line = false;
                start += segment.len() + 1;
            } else {
                start = len;
            }
        }
        reader.consume(len);
    }
}

/// The first `max_lines` lines, each cut to `max_line_bytes` (lossy UTF-8),
/// and whether the file held more.
pub(crate) fn preview(
    path: &Path,
    max_lines: usize,
    max_line_bytes: usize,
) -> Result<(Vec<String>, bool), Error> {
    let mut lines = Vec::new();
    let mut more = false;
    for_each_line_head(path, max_line_bytes, |line| {
        if lines.len() < max_lines {
            lines.push(String::from_utf8_lossy(line).into_owned());
        } else {
            more = true;
        }
    })?;
    Ok((lines, more))
}

/// Whether the file holds any byte that is not ASCII whitespace.
pub(crate) fn has_content(path: &Path) -> Result<bool, Error> {
    let mut reader = open(path)?;
    // loop-ends: end of file or the first non-whitespace byte.
    loop {
        let buf = reader.fill_buf().map_err(|e| read_error(path, &e))?;
        if buf.is_empty() {
            return Ok(false);
        }
        if buf.iter().any(|b| !b.is_ascii_whitespace()) {
            return Ok(true);
        }
        let len = buf.len();
        reader.consume(len);
    }
}

/// The byte offset at which two files first differ (a length difference
/// counts at the shorter one's end), or `None` when they are identical.
pub(crate) fn first_difference(a: &Path, b: &Path) -> Result<Option<u64>, Error> {
    let fa = File::open(a).map_err(|e| read_error(a, &e))?;
    let fb = File::open(b).map_err(|e| read_error(b, &e))?;
    crate::osc::first_stream_difference(fa, fb).map_err(|e| {
        Error::tool(format!(
            "cannot compare {} with {}: {e}",
            a.display(),
            b.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #[test]
    fn malformed_report_is_failed_but_missing_sink_is_verifier_error() {
        let scratch = pbfhogg_test_scratch::scratch();
        let report = scratch.path().join("report");
        std::fs::write(&report, [0xff]).unwrap();
        assert!(matches!(
            super::read_report(&report),
            Err(crate::error::Error::Failed(_))
        ));
        std::fs::remove_file(&report).unwrap();
        assert!(matches!(
            super::read_report(&report),
            Err(crate::error::Error::Tool(_))
        ));
    }

    use super::line_heads;

    fn heads(data: &[u8], head: usize, capacity: usize) -> Vec<Vec<u8>> {
        let reader = std::io::BufReader::with_capacity(capacity, data);
        let mut out = Vec::new();
        line_heads(reader, head, |l| out.push(l.to_vec())).unwrap();
        out
    }

    #[test]
    fn first_difference_checks_content_and_length() {
        let scratch = pbfhogg_test_scratch::scratch();
        let a = scratch.path().join("a");
        let b = scratch.path().join("b");
        std::fs::write(&a, b"abcdef").unwrap();
        std::fs::write(&b, b"abcdefg").unwrap();
        assert_eq!(super::first_difference(&a, &b).unwrap(), Some(6));
        assert_eq!(super::first_difference(&a, &a).unwrap(), None);
        std::fs::write(&b, b"abcxef").unwrap();
        assert_eq!(super::first_difference(&a, &b).unwrap(), Some(3));
    }

    #[test]
    fn line_heads_are_bounded_and_survive_buffer_splits() {
        let data = b"*n1 abc\n*w-22 x\n\nlong line without end";
        for capacity in [1, 2, 3, 7, 64] {
            assert_eq!(
                heads(data, 4, capacity),
                vec![
                    b"*n1 ".to_vec(),
                    b"*w-2".to_vec(),
                    Vec::new(),
                    b"long".to_vec()
                ],
                "capacity {capacity}"
            );
        }
        assert_eq!(heads(b"", 3, 8).len(), 0);
        assert_eq!(heads(b"a\nb", 3, 8).len(), 2);
    }

    #[test]
    fn file_helpers_read_through_fixed_buffers() {
        let scratch = pbfhogg_test_scratch::scratch();
        let a = scratch.path().join("a.txt");
        let blank = scratch.path().join("blank.txt");
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&blank, " \n\n").unwrap();
        assert!(super::has_content(&a).unwrap());
        assert!(!super::has_content(&blank).unwrap());
        let (lines, more) = super::preview(&a, 2, 2).unwrap();
        assert_eq!(lines, vec!["on".to_owned(), "tw".to_owned()]);
        assert!(more);
    }
}
