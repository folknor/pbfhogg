//! A strict reader for pbfhogg's diff summary counts, and the fallback note
//! both diff formats print on stderr.

/// The worker count every judged diff and `diff --format osc` run passes.
/// pbfhogg prints its sequential-fallback note only when more than one
/// worker was asked for, and an automatic count resolves to one on a
/// one-core host, so a fixed count above one makes the note's presence or
/// absence mean the same on every host. The engine is chosen by indexdata
/// alone, never by the worker count, so no engine goes unexercised.
pub(crate) const JOBS: &str = "2";

/// The line pbfhogg prints, at `-j` above one, when an input lacking
/// indexdata sends a diff or derive to its sequential element stream
/// (`crates/lib/src/commands/diff/mod.rs` `compare`, `derive.rs`
/// `derive_changes_to`). Both formats print the same line.
const FALLBACK_NOTE: &str = "Note: Using sequential comparison: missing indexdata";

/// Whether a run's stderr carries the sequential-fallback note, as a whole
/// line. This confirms the indexdata predicate that selects the engine, not
/// entry into the engine itself. A reworded note fails a run that must carry
/// it rather than passing; a path holding a newline could only add a line.
pub(crate) fn fallback_note(stderr: &str) -> bool {
    stderr.lines().any(|l| l == FALLBACK_NOTE)
}

/// Record whether a run's fallback note is as its inputs predict: present
/// on a pair stripped of indexdata, absent on an indexed pair. Output
/// parity between the two pairs holds whichever engine ran, so it cannot
/// show the stripped run took the fallback or the indexed run did not.
pub(crate) fn record_fallback_note(
    harness: &mut crate::harness::Harness,
    label: &str,
    stripped: bool,
    note: bool,
) {
    let ok = note == stripped;
    let say = |present: bool| if present { "present" } else { "absent" };
    harness.msg(&format!(
        "  {label}: fallback note {} (expected {}) {}",
        say(note),
        say(stripped),
        if ok { "PASS" } else { "FAIL" }
    ));
    harness.record(
        &format!(
            "{label} fallback note {} where its inputs predict it {}",
            say(note),
            say(stripped)
        ),
        crate::harness::Verdict::from_pass(ok),
    );
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
    pub common: u64,
}
pub(crate) fn parse_pbfhogg_summary(text: &str) -> Option<Counts> {
    let mut found = None;
    for line in text.lines() {
        if let Some(counts) = parse_summary_line(line.trim()) {
            if found.is_some() {
                return None;
            }
            found = Some(counts);
        }
    }
    found
}

fn number(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn parse_summary_line(line: &str) -> Option<Counts> {
    if let Some(rest) = line.strip_prefix("Files are identical (") {
        let common = number(rest.strip_suffix(" common elements)")?)?;
        return Some(Counts {
            common,
            ..Counts::default()
        });
    }
    let (total, rest) = line.split_once(" differences: ")?;
    let (created, rest) = rest.split_once(" created, ")?;
    let (modified, rest) = rest.split_once(" modified, ")?;
    let (deleted, rest) = rest.split_once(" deleted (")?;
    let counts = Counts {
        created: number(created)?,
        modified: number(modified)?,
        deleted: number(deleted)?,
        common: number(rest.strip_suffix(" common)")?)?,
    };
    let total = number(total)?;
    (total > 0
        && Some(total)
            == counts
                .created
                .checked_add(counts.modified)?
                .checked_add(counts.deleted))
    .then_some(counts)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn pbfhogg_summaries_parse_strictly() {
        assert_eq!(
            parse_pbfhogg_summary(
                "note: something\n10321 differences: 5589 created, 3489 modified, 1243 deleted (59147550 common)\n"
            ),
            Some(Counts {
                created: 5589,
                modified: 3489,
                deleted: 1243,
                common: 59_147_550
            })
        );
        assert_eq!(
            parse_pbfhogg_summary("Files are identical (59152282 common elements)"),
            Some(Counts {
                common: 59_152_282,
                ..Counts::default()
            })
        );
        for text in [
            "10 differences: 5 created, 3 modified, 1 deleted (5 common)",
            "Files are identical (1 common elements)\nFiles are identical (1 common elements)",
            "0 differences: 0 created, 0 modified, 0 deleted (5 common)",
            "",
        ] {
            assert_eq!(parse_pbfhogg_summary(text), None, "{text}");
        }
    }

    #[test]
    fn the_fallback_note_is_one_exact_line() {
        assert!(fallback_note(&format!(
            "{FALLBACK_NOTE}\n1 differences: x\n"
        )));
        assert!(fallback_note(&format!("a\n{FALLBACK_NOTE}")));
        for text in [
            "",
            "Note: Using sequential comparison",
            "Note: Using sequential comparison: missing indexdata (old)",
            " Note: Using sequential comparison: missing indexdata",
            "Using sequential comparison: missing indexdata",
        ] {
            assert!(!fallback_note(text), "{text}");
        }
    }
}
