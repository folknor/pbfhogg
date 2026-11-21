//! Bounded-memory readers over a step's stdout file.
//!
//! A listing step (a diff of two PBFs, an OPL merge) writes its listing to
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

/// The first `max_lines` lines, each at most its first `head` bytes (newline
/// excluded, a last line without one counted), assembled across buffer
/// refills, and whether any byte follows them. Reading stops at the first
/// byte past the last line kept, so a long listing is not read to its end.
fn line_heads(
    mut reader: impl BufRead,
    head: usize,
    max_lines: usize,
) -> io::Result<(Vec<Vec<u8>>, bool)> {
    let mut lines = Vec::new();
    let mut current: Vec<u8> = Vec::with_capacity(head);
    let mut in_line = false;
    // loop-ends: end of input, or the first byte past `max_lines` lines.
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if in_line {
                lines.push(current);
            }
            return Ok((lines, false));
        }
        if lines.len() == max_lines {
            return Ok((lines, true));
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
                lines.push(std::mem::take(&mut current));
                in_line = false;
                start += segment.len() + 1;
                if lines.len() == max_lines && start < len {
                    return Ok((lines, true));
                }
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
    let (lines, more) =
        line_heads(open(path)?, max_line_bytes, max_lines).map_err(|e| read_error(path, &e))?;
    let lines = lines
        .iter()
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect();
    Ok((lines, more))
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
        let (lines, more) = line_heads(reader, head, usize::MAX).unwrap();
        assert!(!more);
        lines
    }

    /// A reader of `data` that fails any read once `limit` bytes are out.
    struct Fenced<'a> {
        data: &'a [u8],
        limit: usize,
    }

    impl std::io::Read for Fenced<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.limit == 0 {
                return Err(std::io::Error::other("read past the fence"));
            }
            let n = out.len().min(self.limit).min(self.data.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            self.limit -= n;
            Ok(n)
        }
    }

    #[test]
    fn line_heads_stop_at_the_first_byte_past_the_kept_lines() {
        // "a\nb\n" kept, then one byte of a huge unterminated third line.
        let data = b"a\nb\ncccccccccccccccccccc";
        for capacity in [1, 2, 3, 5, 64] {
            let fenced = Fenced { data, limit: 5 };
            let reader = std::io::BufReader::with_capacity(capacity, fenced);
            let (lines, more) = line_heads(reader, 8, 2).unwrap();
            assert_eq!(
                lines,
                vec![b"a".to_vec(), b"b".to_vec()],
                "capacity {capacity}"
            );
            assert!(more, "capacity {capacity}");
        }
        let (lines, more) = line_heads(&b"a\nb\n"[..], 8, 2).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(!more);
        let (lines, more) = line_heads(&b"a\nb\n\n"[..], 8, 2).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(more, "an empty extra line is more");
        let (lines, more) = line_heads(&b"x"[..], 8, 0).unwrap();
        assert!(lines.is_empty());
        assert!(more);
        let (_, more) = line_heads(&b""[..], 8, 0).unwrap();
        assert!(!more);
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
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        let (lines, more) = super::preview(&a, 2, 2).unwrap();
        assert_eq!(lines, vec!["on".to_owned(), "tw".to_owned()]);
        assert!(more);
    }
}
