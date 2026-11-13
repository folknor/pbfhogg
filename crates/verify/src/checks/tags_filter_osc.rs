//! Verify: tags-filter --input-kind osc - pbfhogg's change-file filter
//! against `osmium tags-filter -R` on osmium's reading of the same file.
//!
//! osmium's tags-filter takes no change file (it writes actions from
//! versions, and without `-R` follows references), but with `-R` it is a
//! pure predicate on each object's own tags, in input order, and its manual
//! admits it on a file of several versions per object under `-R`, which is
//! what osmium's OPL of a change file is. So the reference runs on that OPL
//! (the content side of the input's change listing,
//! [`crate::verify_change_listing`]) with the expressions, and the expected
//! output is built from the input listing in lockstep with osmium's: a
//! create or modify is kept exactly when it is osmium's next line (identical
//! lines get identical decisions, so taking the first match is exact),
//! every delete is kept, and osmium's output must be used up. pbfhogg's
//! output must then be those changes in one create, one modify and one
//! delete section, each holding its node, way and relation changes in that
//! order, each kind in input order, compared record for record with a
//! delete held to its input body, as pbfhogg and osmium write it. The
//! cases are the given change file's, so how many bodied deletes are
//! checked depends on it. A
//! `--jobs 1` run must write the same content as the automatic one, which
//! pbfhogg promises at any `-j`.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::{Harness, Verdict};
use crate::osc::OscAction;
use crate::reference::ToolRun;
use crate::verify_change_listing::{self as listing, ListingReader};

/// Each case's directory name and its expressions.
const CASES: &[(&str, &[&str])] = &[
    ("highway-primary", &["highway=primary"]),
    ("amenity-restaurant", &["amenity=restaurant"]),
    ("w-highway-primary", &["w/highway=primary"]),
    ("n-amenity-r-route", &["n/amenity", "r/type=route"]),
];

/// What the expected outputs held, over every case.
#[derive(Default)]
struct Kept {
    creates: u64,
    modifies: u64,
    deletes: u64,
    /// Deletes osmium's filter matched too (a delete carrying its old tags).
    matched_deletes: u64,
    deletes_with_body: u64,
}

/// Build the expected listing of one case at `dir/expected.changes`.
fn expected(input: &Path, osmium: &Path, dir: &Path, kept: &mut Kept) -> Result<(), Error> {
    let mut groups = Vec::new();
    let mut writers = Vec::new();
    for action in ["create", "modify", "delete"] {
        for kind in ["node", "way", "relation"] {
            let path = dir.join(format!("expected-{action}-{kind}.changes"));
            writers.push(BufWriter::new(File::create(&path)?));
            groups.push(path);
        }
    }
    let mut reader = ListingReader::open(input)?;
    let mut matches = BufReader::new(File::open(osmium)?);
    let mut next = String::new();
    let mut read_next = |next: &mut String| -> Result<(), Error> {
        next.clear();
        matches.read_line(next)?;
        if !next.is_empty() && !next.ends_with('\n') {
            return Err(Error::Failed(format!(
                "{} ends inside a line",
                osmium.display()
            )));
        }
        Ok(())
    };
    read_next(&mut next)?;
    while let Some((_, change)) = reader.next_record()? {
        let matched = next.strip_suffix('\n') == Some(change.opl.as_str());
        if matched {
            read_next(&mut next)?;
        }
        let keep = match change.action {
            OscAction::Delete => {
                kept.deletes += 1;
                kept.matched_deletes += u64::from(matched);
                kept.deletes_with_body += u64::from(change.has_body());
                true
            }
            OscAction::Create => {
                kept.creates += u64::from(matched);
                matched
            }
            OscAction::Modify => {
                kept.modifies += u64::from(matched);
                matched
            }
        };
        if keep {
            let action = match change.action {
                OscAction::Create => 0,
                OscAction::Modify => 1,
                OscAction::Delete => 2,
            };
            writers[action * 3 + change.kind.index()].write_all(change.record().as_bytes())?;
        }
    }
    if !next.is_empty() {
        return Err(Error::Failed(format!(
            "osmium tags-filter kept {:?}, which is no change of the input past where the \
             lockstep reached",
            next.trim_end()
        )));
    }
    for mut writer in writers {
        writer.flush()?;
    }
    let groups: Vec<&Path> = groups.iter().map(std::path::PathBuf::as_path).collect();
    listing::concatenate(&groups, &dir.join("expected.changes"))
}

/// Cross-validate `pbfhogg tags-filter --input-kind osc` on the change file
/// `osc`.
pub fn run(harness: &mut Harness, osc: &Path) -> Result<Option<String>, Error> {
    let outdir = harness.subdir("tags-filter-osc")?;
    harness.msg("=== verify tags-filter --input-kind osc ===");
    harness.msg(&format!("  input: {}", osc.display()));
    let input = listing::listing(harness, osc, &outdir, "input")?;
    let mut kept = Kept::default();

    for (name, expressions) in CASES {
        let what = format!("tags-filter --input-kind osc {}", expressions.join(" "));
        harness.msg(&format!("--- {what} ---"));
        let dir = outdir.join(name);
        std::fs::create_dir(&dir)?;

        let osmium_out = dir.join("osmium.opl");
        let captured = harness.reference(
            &ToolRun::osmium("tags-filter-osc.1")
                .arg("tags-filter")
                .input(&input.opl)
                .args(expressions)
                .args(&["-R", "-F", "opl", "-f", "opl", "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &dir.join("osmium.stdout"),
        )?;
        harness.require_success(&captured, "osmium tags-filter -R")?;
        expected(&input.path, &osmium_out, &dir, &mut kept)?;

        let argv = |out: &Path, jobs: bool| -> Result<PbfArgv, Error> {
            Ok(PbfArgv::new("tags-filter")
                .arg("--input-kind")
                .arg("osc")
                .input(osc)
                .args_from(expressions.iter().copied())
                .output(out)
                .arg_if(jobs, "--jobs")
                .arg_if(jobs, "1"))
        };
        let out = dir.join("pbfhogg.osc.gz");
        let captured = harness.pbfhogg_discard(argv(&out, false)?)?;
        harness.require_success(&captured, "pbfhogg tags-filter --input-kind osc")?;
        let actual = listing::listing(harness, &out, &dir, "pbfhogg")?;
        let cmp = listing::compare(&dir.join("expected.changes"), &actual.path)?;
        cmp.report(
            harness,
            &format!("{what} as osmium's selection, deletes kept"),
        );
        harness.record(&what, Verdict::from_pass(cmp.is_equal()));

        let serial = dir.join("pbfhogg-j1.osc");
        let captured = harness.pbfhogg_discard(argv(&serial, true)?)?;
        harness.require_success(&captured, "pbfhogg tags-filter --input-kind osc --jobs 1")?;
        match listing::same_content(&out, &serial)? {
            None => harness.msg(&format!("  {what} --jobs 1: same content PASS")),
            Some(why) => {
                harness.msg(&format!("  {what} --jobs 1: {why} FAIL"));
                harness.fail(format!("{what} --jobs 1 content differs: {why}"));
            }
        }
    }

    let cases = CASES
        .iter()
        .map(|(_, e)| e.join(" "))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(format!(
        "for {cases}: every delete kept and each create and modify kept exactly when osmium \
         tags-filter -R keeps it from osmium's reading of the change file, in create, modify \
         and delete sections of node, way and relation changes in input order, comparing action \
         labels, metadata attributes (an empty one held to be omitted) and osmium's decoding \
         record for record, a delete held to its input body, section boundaries not compared, \
         --jobs 1 as the automatic workers; over the cases {} creates, {} modifies and {} \
         deletes kept, {} of the deletes matched by osmium's filter and {} bodied deletes \
         checked",
        kept.creates, kept.modifies, kept.deletes, kept.matched_deletes, kept.deletes_with_body
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::osc::{OscKind, OscMeta};
    use crate::verify_change_listing::Change;

    #[test]
    fn osc_filter_expected_keeps_deletes_and_groups_selected_changes() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let changes = [
            Change {
                action: OscAction::Modify,
                kind: OscKind::Way,
                id: 2,
                meta: OscMeta::default(),
                opl: "w2 v0 N1 T".into(),
            },
            Change {
                action: OscAction::Delete,
                kind: OscKind::Node,
                id: 3,
                meta: OscMeta::default(),
                opl: "n3 v0 x y T".into(),
            },
            Change {
                action: OscAction::Create,
                kind: OscKind::Node,
                id: 1,
                meta: OscMeta::default(),
                opl: "n1 v0 x1 y2 Tamenity=restaurant".into(),
            },
            Change {
                action: OscAction::Modify,
                kind: OscKind::Node,
                id: 4,
                meta: OscMeta::default(),
                opl: "n4 v0 x1 y2 T".into(),
            },
            Change {
                action: OscAction::Delete,
                kind: OscKind::Way,
                id: 5,
                meta: OscMeta::default(),
                opl: "w5 v0 N1 Thighway=primary".into(),
            },
        ];
        let input = dir.join("input.changes");
        std::fs::write(
            &input,
            changes.iter().map(Change::record).collect::<String>(),
        )
        .unwrap();
        let matches = dir.join("matched.opl");
        std::fs::write(
            &matches,
            format!(
                "{}\n{}\n{}\n",
                changes[0].opl, changes[2].opl, changes[4].opl
            ),
        )
        .unwrap();
        let mut kept = Kept::default();
        expected(&input, &matches, dir, &mut kept).unwrap();
        let actual = std::fs::read_to_string(dir.join("expected.changes")).unwrap();
        assert_eq!(
            actual,
            [2, 0, 1, 4]
                .iter()
                .map(|i| changes[*i].record())
                .collect::<String>()
        );
        assert_eq!(
            (
                kept.creates,
                kept.modifies,
                kept.deletes,
                kept.matched_deletes,
                kept.deletes_with_body
            ),
            (1, 1, 2, 1, 1)
        );
        std::fs::write(&matches, "n99 v0 T\n").unwrap();
        assert!(expected(&input, &matches, dir, &mut Kept::default()).is_err());
        std::fs::write(&matches, "n99 v0 T").unwrap();
        assert!(expected(&input, &matches, dir, &mut Kept::default()).is_err());
    }
}
