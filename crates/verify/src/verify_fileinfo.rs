//! What verify reads from `osmium fileinfo -e -j --no-crc`: osmium's own
//! reading of a file's decoded objects, a reference run like any other
//! (`crate::reference`), its JSON report on stdout written to a file
//! in the check's directory and read back for the fields below, every other
//! value skipped.
//!
//! - `data.objects_ordered`: false when an object's ID steps down from the
//!   previous one of its kind under libosmium's `id_order` (0, the negatives
//!   by ascending absolute value, the positives: the canonical order pbfhogg
//!   uses) or its kind comes after a later kind's.
//! - `data.multiple_versions`: true when an object repeats the previous
//!   object's kind and ID. osmium prints it only on an ordered file; on an
//!   ordered file its absence is a malformed report, never false.
//! - `data.count`, `data.minid` and `data.maxid` per kind (node, way,
//!   relation); minimum and maximum mean nothing for a kind with no object.
//!
//! One run of a file serves every check that reads it: the run id, argv and
//! sink name are fixed here, so the reference cache keys it by the file's
//! content alone.

#![allow(
    dead_code,
    reason = "shared verifier helpers consumed by the remaining family slices"
)]

use std::path::Path;

use crate::error::Error;
use crate::harness::Harness;
use crate::reference::ToolRun;

/// osmium's reading of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub ordered: bool,
    /// `None` where osmium does not say: on an unordered file.
    pub multiple_versions: Option<bool>,
    /// Objects per kind: node, way, relation.
    pub counts: [u64; 3],
    pub min_ids: [i64; 3],
    pub max_ids: [i64; 3],
}

impl FileInfo {
    /// Whether the objects strictly ascend by kind and then ID: ordered and
    /// no ID repeated.
    pub fn strictly_ordered(&self) -> bool {
        self.ordered && self.multiple_versions == Some(false)
    }

    /// The lowest ID of a kind with objects that is negative, as
    /// `(kind index, id)`.
    pub fn negative_id(&self) -> Option<(usize, i64)> {
        (0..3).find_map(|kind| {
            (self.counts[kind] > 0 && self.min_ids[kind] < 0).then_some((kind, self.min_ids[kind]))
        })
    }

    /// The lowest and highest ID of `kind`, `None` without objects.
    pub fn range(&self, kind: usize) -> Option<(i64, i64)> {
        (self.counts[kind] > 0).then_some((self.min_ids[kind], self.max_ids[kind]))
    }
}

#[derive(serde::Deserialize)]
struct Report {
    data: Data,
}

#[derive(serde::Deserialize)]
struct Data {
    objects_ordered: bool,
    #[serde(default)]
    multiple_versions: Option<bool>,
    count: Kinds<u64>,
    minid: Kinds<i64>,
    maxid: Kinds<i64>,
}

#[derive(serde::Deserialize)]
struct Kinds<T> {
    nodes: T,
    ways: T,
    relations: T,
}

impl<T: Copy> Kinds<T> {
    fn array(&self) -> [T; 3] {
        [self.nodes, self.ways, self.relations]
    }
}

fn parse(reader: impl std::io::Read) -> Result<FileInfo, String> {
    let report: Report = serde_json::from_reader(reader).map_err(|e| e.to_string())?;
    let data = report.data;
    if data.objects_ordered && data.multiple_versions.is_none() {
        return Err("an ordered file without data.multiple_versions".into());
    }
    Ok(FileInfo {
        ordered: data.objects_ordered,
        multiple_versions: data.multiple_versions,
        counts: data.count.array(),
        min_ids: data.minid.array(),
        max_ids: data.maxid.array(),
    })
}

/// osmium fileinfo of `pbf`, its report at `report` (a `.json` file in the
/// check's directory). Exit 0 only; a report that does not read is an error.
pub(crate) fn fileinfo(
    harness: &mut Harness,
    pbf: &Path,
    report: &Path,
) -> Result<FileInfo, Error> {
    let run = ToolRun::osmium("fileinfo-e.1")
        .args(&["fileinfo", "-e", "-j", "--no-crc"])
        .input(pbf);
    let captured = harness.reference(&run, report)?;
    harness.require_success(&captured, "osmium fileinfo -e")?;
    let file = std::fs::File::open(report)
        .map_err(|e| Error::Failed(format!("cannot open {}: {e}", report.display())))?;
    let info = parse(std::io::BufReader::new(file)).map_err(|why| {
        Error::Failed(format!(
            "cannot read the osmium fileinfo report {}: {why}",
            report.display()
        ))
    })?;
    harness.msg(&format!(
        "  osmium fileinfo {}: {} nodes, {} ways, {} relations, {}",
        pbf.display(),
        info.counts[0],
        info.counts[1],
        info.counts[2],
        match (info.ordered, info.multiple_versions) {
            (false, _) => "not ordered",
            (true, Some(true)) => "ordered, an ID repeated",
            (true, _) => "ordered, no ID repeated",
        }
    ));
    Ok(info)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const ORDERED: &str = r#"{"file":{"name":"x","format":"PBF","compression":"none","size":1},
        "header":{"boxes":[],"with_history":false,"option":{"generator":"x"}},
        "data":{"bbox":[1.0,1.0,1.1,1.1],"timestamp":{"first":"","last":"1970-01-01T00:00:01Z"},
          "objects_ordered":true,"multiple_versions":false,
          "count":{"changesets":0,"nodes":2,"ways":1,"relations":0},
          "minid":{"changesets":0,"nodes":1,"ways":10,"relations":0},
          "maxid":{"changesets":0,"nodes":2,"ways":10,"relations":0},
          "buffers":{"count":2,"size":192,"capacity":131072},
          "metadata":{"all_objects":{"version":true}}}}"#;

    #[test]
    fn an_ordered_report_reads_its_counts_ranges_and_order() {
        let info = parse(ORDERED.as_bytes()).unwrap();
        assert!(info.strictly_ordered());
        assert_eq!(info.counts, [2, 1, 0]);
        assert_eq!(info.range(0), Some((1, 2)));
        assert_eq!(info.range(2), None);
        assert_eq!(info.negative_id(), None);
    }

    #[test]
    fn an_unordered_report_needs_no_repeat_flag_and_an_ordered_one_does() {
        let unordered = ORDERED.replace(
            r#""objects_ordered":true,"multiple_versions":false,"#,
            r#""objects_ordered":false,"#,
        );
        let info = parse(unordered.as_bytes()).unwrap();
        assert!(!info.strictly_ordered());
        assert_eq!(info.multiple_versions, None);
        let malformed = ORDERED.replace(r#""multiple_versions":false,"#, "");
        assert!(parse(malformed.as_bytes()).is_err());
        let repeated = ORDERED.replace(
            r#""multiple_versions":false"#,
            r#""multiple_versions":true"#,
        );
        assert!(!parse(repeated.as_bytes()).unwrap().strictly_ordered());
    }

    #[test]
    fn a_negative_minimum_counts_only_for_a_kind_with_objects() {
        let negative = ORDERED.replace(
            r#""minid":{"changesets":0,"nodes":1"#,
            r#""minid":{"changesets":0,"nodes":-4"#,
        );
        assert_eq!(
            parse(negative.as_bytes()).unwrap().negative_id(),
            Some((0, -4))
        );
        let empty_kind = ORDERED.replace(
            r#""ways":10,"relations":0},"maxid""#,
            r#""ways":10,"relations":-3},"maxid""#,
        );
        assert_eq!(parse(empty_kind.as_bytes()).unwrap().negative_id(), None);
        assert!(
            parse(
                ORDERED
                    .replace(r#""nodes":2,"ways":1"#, r#""ways":1"#)
                    .as_bytes()
            )
            .is_err()
        );
    }
}
