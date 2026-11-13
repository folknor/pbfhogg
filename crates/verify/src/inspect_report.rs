//! What the verifier reads from `pbfhogg inspect --json`.
//!
//! The report is written to a file beside the inspected output
//! (`<stem>.inspect.json`, or `<stem>.inspect-extended.json` under
//! `--extended`) and read back by a streaming deserializer that keeps only
//! the fields below. A report missing a read field is refused; a field whose
//! meaning changes under a new schema is not detected. The fields read include counts,
//! optional features and standard order.
use std::path::{Path, PathBuf};

use crate::argv::PbfArgv;
use crate::error::Error;
use crate::harness::Harness;

pub(crate) const SORT_TYPE_THEN_ID: &str = "Sort.Type_then_ID";

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct InspectSummary {
    header: Header,
    elements: Elements,
    ordering: Ordering,
    /// Present only under `--extended`.
    #[serde(default)]
    data: Option<ExtendedData>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct Header {
    required_features: Vec<String>,
    optional_features: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
struct Elements {
    nodes: u64,
    ways: u64,
    relations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
struct Ordering {
    standard: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct ExtendedData {
    /// `[min_lon, min_lat, max_lon, max_lat]`, null for a file without nodes.
    bbox: Option<[f64; 4]>,
    timestamp: Option<Timestamps>,
    objects_ordered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct Timestamps {
    first: String,
    last: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InspectCounts {
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

impl InspectSummary {
    pub(crate) fn counts(&self) -> InspectCounts {
        InspectCounts {
            nodes: self.elements.nodes,
            ways: self.elements.ways,
            relations: self.elements.relations,
        }
    }
    pub(crate) fn optional_sorted(&self) -> bool {
        self.optional_feature(SORT_TYPE_THEN_ID)
    }
    pub(crate) fn optional_feature(&self, feature: &str) -> bool {
        self.header.optional_features.iter().any(|f| f == feature)
    }
    pub(crate) fn standard_order(&self) -> bool {
        self.ordering.standard
    }

    /// Whether the header declares `Sort.Type_then_ID` in either list.
    pub(crate) fn declares_sorted(&self) -> bool {
        self.header
            .required_features
            .iter()
            .chain(&self.header.optional_features)
            .any(|f| f == SORT_TYPE_THEN_ID)
    }

    /// The computed element order (standard ordering and per-kind ID
    /// monotonicity), `None` unless the report is extended.
    pub(crate) fn objects_ordered(&self) -> Option<bool> {
        self.data.as_ref().map(|d| d.objects_ordered)
    }

    /// The summary as operator lines, each prefixed with `label`.
    pub(crate) fn lines(&self, label: &str) -> Vec<String> {
        let e = &self.elements;
        let mut features: Vec<&str> = self
            .header
            .required_features
            .iter()
            .chain(&self.header.optional_features)
            .map(String::as_str)
            .collect();
        features.dedup();
        let order = if self.ordering.standard {
            "standard"
        } else {
            "NON-STANDARD"
        };
        let mut lines = vec![
            format!(
                "{label}: elements: {} nodes, {} ways, {} relations",
                e.nodes, e.ways, e.relations
            ),
            format!("{label}: features: {}", features.join(", ")),
            format!("{label}: ordering: {order}"),
        ];
        if let Some(data) = &self.data {
            let ordered = if data.objects_ordered { "yes" } else { "no" };
            lines.push(format!("{label}: ordered: {ordered}"));
            if let Some([min_lon, min_lat, max_lon, max_lat]) = data.bbox {
                lines.push(format!(
                    "{label}: data bbox: {min_lon},{min_lat},{max_lon},{max_lat}"
                ));
            }
            if let Some(t) = &data.timestamp {
                lines.push(format!("{label}: timestamps: {} .. {}", t.first, t.last));
            }
        }
        lines
    }
}

fn parse_summary(reader: impl std::io::Read) -> serde_json::Result<InspectSummary> {
    serde_json::from_reader(reader)
}

/// Where the report of `pbf` is written: beside it.
pub(crate) fn report_path(pbf: &Path, extended: bool) -> PathBuf {
    pbf.with_extension(if extended {
        "inspect-extended.json"
    } else {
        "inspect.json"
    })
}

/// Run `pbfhogg inspect --json [--extended]` on `pbf`, its report written to
/// [`report_path`], and read the summary back. A non-zero exit or an
/// unreadable report is an error; an extended report without
/// `data.objects_ordered` is refused, never read as unordered.
pub(crate) fn inspect(
    h: &mut Harness,
    pbf: &Path,
    extended: bool,
) -> Result<InspectSummary, Error> {
    inspect_into(h, pbf, &report_path(pbf, extended), extended)
}

pub(crate) fn inspect_into(
    h: &mut Harness,
    pbf: &Path,
    report: &Path,
    extended: bool,
) -> Result<InspectSummary, Error> {
    let argv = PbfArgv::new("inspect")
        .arg("--json")
        .arg_if(extended, "--extended")
        .input(pbf);
    let ended = h.pbfhogg(argv, report)?;
    h.require_success(&ended, "pbfhogg inspect --json")?;
    read_report(report, extended)
}

fn read_report(report: &Path, extended: bool) -> Result<InspectSummary, Error> {
    let file = std::fs::File::open(report)
        .map_err(|e| Error::tool(format!("cannot open {}: {e}", report.display())))?;
    let summary = parse_summary(std::io::BufReader::new(file)).map_err(|e| {
        Error::Failed(format!(
            "cannot read the inspect report {}: {e}",
            report.display()
        ))
    })?;
    if extended && summary.data.is_none() {
        return Err(Error::Failed(format!(
            "the inspect report {} has no data.objects_ordered although --extended was passed",
            report.display()
        )));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// pbfhogg's schema_version 2 shape, trimmed to what is read plus
    /// neighbours the reader must skip.
    const PLAIN: &str = r#"{"schema_version":2,"file":"x.osm.pbf","file_size":10,
        "header":{"writing_program":"pbfhogg","required_features":["OsmSchema-V0.6","DenseNodes"],
          "optional_features":["Sort.Type_then_ID"],"bbox":null},
        "indexed":true,
        "elements":{"nodes":5,"tagged_nodes":1,"ways":2,"relations":0,"total":7},
        "ordering":{"standard":false,"segment_count":4,"segments_head":[]},
        "id_ranges":null,"locations":null}"#;

    #[test]
    fn plain_report_reads_flag_and_ordering_and_skips_the_rest() {
        let s = parse_summary(PLAIN.as_bytes()).unwrap();
        assert!(s.declares_sorted());
        assert_eq!(s.objects_ordered(), None);
        assert!(
            s.lines("p")
                .iter()
                .any(|l| l == "p: ordering: NON-STANDARD")
        );
    }

    #[test]
    fn sorted_flag_is_an_exact_feature_not_a_substring() {
        let text = PLAIN
            .replace(
                r#""optional_features":["Sort.Type_then_ID"]"#,
                r#""optional_features":[]"#,
            )
            .replace(r#""file":"x.osm.pbf""#, r#""file":"Sort.Type_then_ID.pbf""#);
        assert!(!parse_summary(text.as_bytes()).unwrap().declares_sorted());
    }

    #[test]
    fn counts_and_optional_claims_preserve_their_meaning() {
        let required = PLAIN
            .replace(
                r#""optional_features":["Sort.Type_then_ID"]"#,
                r#""optional_features":["LocationsOnWays"]"#,
            )
            .replace(r#""DenseNodes"]"#, r#""DenseNodes","Sort.Type_then_ID"]"#);
        let summary = parse_summary(required.as_bytes()).unwrap();
        assert_eq!(
            summary.counts(),
            InspectCounts {
                nodes: 5,
                ways: 2,
                relations: 0
            }
        );
        assert!(summary.declares_sorted());
        assert!(!summary.optional_sorted());
        assert!(summary.optional_feature("LocationsOnWays"));
        assert!(!summary.optional_feature("LocationsOn"));
        assert!(!summary.standard_order());
        assert!(parse_summary(PLAIN.as_bytes()).unwrap().optional_sorted());
    }

    #[test]
    fn extended_report_carries_the_computed_order() {
        let text = PLAIN.replace(
            r#""locations":null}"#,
            r#""locations":null,"data":{"bbox":[8.0,54.5,15.2,57.8],
              "timestamp":{"first":"2010-01-01T00:00:00Z","last":"2026-10-01T00:00:00Z"},
              "objects_ordered":false}}"#,
        );
        let s = parse_summary(text.as_bytes()).unwrap();
        assert_eq!(s.objects_ordered(), Some(false));
        assert!(s.lines("p").contains(&"p: ordered: no".to_owned()));
    }

    #[test]
    fn missing_counts_are_refused_not_zero() {
        let text = PLAIN.replace(r#""relations":0,"#, "");
        assert!(parse_summary(text.as_bytes()).is_err());
    }

    #[test]
    fn an_extended_report_without_order_is_refused_with_its_path() {
        let scratch = pbfhogg_test_scratch::scratch();
        let path = scratch.path().join("out.inspect-extended.json");
        std::fs::write(&path, PLAIN).unwrap();
        let err = read_report(&path, true).unwrap_err().to_string();
        assert!(err.contains("out.inspect-extended.json"), "{err}");
        assert!(read_report(&path, false).is_ok());
    }

    #[test]
    fn report_sits_beside_the_output() {
        assert_eq!(
            report_path(Path::new("/v/sort/pbfhogg.osm.pbf"), true),
            Path::new("/v/sort/pbfhogg.osm.inspect-extended.json")
        );
    }
}
