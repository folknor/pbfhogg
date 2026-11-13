//! Strict readers for pbfhogg and osmium diff summary counts.
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

/// osmium's `--summary` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OsmiumSummary {
    pub left: u64,
    pub right: u64,
    pub same: u64,
    pub different: u64,
}

/// Exactly one `Summary: left=N right=N same=N different=N` line.
pub(crate) fn parse_osmium_summary(text: &str) -> Option<OsmiumSummary> {
    let mut found = None;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Summary: ") else {
            continue;
        };
        let mut fields = rest.split(' ');
        let mut field = |name: &str| number(fields.next()?.strip_prefix(name)?);
        let summary = OsmiumSummary {
            left: field("left=")?,
            right: field("right=")?,
            same: field("same=")?,
            different: field("different=")?,
        };
        if fields.next().is_some() || found.replace(summary).is_some() {
            return None;
        }
    }
    found
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
    fn osmium_summaries_parse_strictly() {
        assert_eq!(
            parse_osmium_summary("Summary: left=4 right=4 same=1 different=3\n"),
            Some(OsmiumSummary {
                left: 4,
                right: 4,
                same: 1,
                different: 3
            })
        );
        assert_eq!(parse_osmium_summary("Summary: left=4 right=4 same=1"), None);
        assert_eq!(
            parse_osmium_summary("Summary: left=4 right=4 same=1 different=3 x=1"),
            None
        );
        assert_eq!(parse_osmium_summary(""), None);
    }
}
