//! Regions and declared file effects for extract verification.
use crate::{argv::PbfArgv, error::Error};
use pbfhogg_devkit::Bbox;
use std::path::{Path, PathBuf};
#[derive(Debug, Clone)]
pub(crate) struct PolygonInput {
    pub name: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone)]
pub(crate) enum Region {
    Bbox(Bbox),
    Polygon(PolygonInput),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtractStrategy {
    Simple,
    Complete,
    Smart,
}
impl ExtractStrategy {
    pub fn all() -> &'static [Self] {
        &[Self::Simple, Self::Complete, Self::Smart]
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Complete => "complete",
            Self::Smart => "smart",
        }
    }
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Simple => Some("--simple"),
            Self::Complete => None,
            Self::Smart => Some("--smart"),
        }
    }
    pub fn osmium_name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Complete => "complete_ways",
            Self::Smart => "smart",
        }
    }
}
pub(crate) enum PbfhoggCommand {
    Extract { strategy: ExtractStrategy },
}
impl PbfhoggCommand {
    pub fn extract_args_for(
        &self,
        pbf: &Path,
        region: &Region,
        output: &Path,
    ) -> Result<PbfArgv, Error> {
        let Self::Extract { strategy } = self;
        let mut argv = PbfArgv::new("extract").input(pbf);
        if let Some(flag) = strategy.flag() {
            argv = argv.arg(flag);
        }
        argv = match region {
            Region::Bbox(bbox) => argv.arg(bbox.to_pbfhogg_arg()),
            Region::Polygon(polygon) => argv.arg("--polygon").input(&polygon.path),
        };
        Ok(argv.output(output))
    }
}
pub(crate) struct NamedRegion {
    pub(crate) stem: String,
    pub(crate) region: Region,
}

impl NamedRegion {
    pub(crate) fn output_name(&self) -> String {
        format!("{}.osm.pbf", self.stem)
    }
}

/// The ordered regions of a generated multi-extract config: `strip-<i>` for
/// each strip, then `polygon-<name>` for each polygon. Polygon names match
/// `[A-Za-z0-9_-]+` and carry the prefix, so no two stems collide.
pub(crate) fn multi_extract_regions(
    strips: &[Bbox],
    polygons: &[PolygonInput],
) -> Vec<NamedRegion> {
    let strips = strips.iter().enumerate().map(|(i, strip)| NamedRegion {
        stem: format!("strip-{i}"),
        region: Region::Bbox(strip.clone()),
    });
    let polygons = polygons.iter().map(|polygon| NamedRegion {
        stem: format!("polygon-{}", polygon.name),
        region: Region::Polygon(polygon.clone()),
    });
    strips.chain(polygons).collect()
}

/// A multi-extract config, the outputs it names and the polygon files it
/// reads, from one construction, so the reservation a launch makes cannot
/// disagree with the document.
#[derive(Debug, Clone)]
pub(crate) struct MultiExtractConfig {
    pub(crate) json: String,
    pub(crate) outputs: Vec<PathBuf>,
    pub(crate) reads: Vec<PathBuf>,
}

/// Encode multi-extract regions for a resolved output path. Both the measured
/// command and `verify multi-extract` use serde_json for escaping. A bbox
/// region is written as its coordinates, a polygon as a `polygon_file` entry
/// naming its absolute path, so pbfhogg's relative-path rule never applies.
pub(crate) fn multi_extract_config(
    output_dir: &Path,
    regions: &[NamedRegion],
) -> Result<MultiExtractConfig, Error> {
    if regions.is_empty() {
        return Err(Error::Tool(
            "multi-extract requires at least 1 region".into(),
        ));
    }
    let output_dir_str = utf8(output_dir)?;
    let mut reads = Vec::new();
    let mut extracts = Vec::with_capacity(regions.len());
    for named in regions {
        let entry = match &named.region {
            Region::Bbox(bbox) => serde_json::json!({
                "output": named.output_name(),
                "bbox": bbox.coordinates(),
            }),
            Region::Polygon(polygon) => {
                reads.push(polygon.path.clone());
                serde_json::json!({
                    "output": named.output_name(),
                    "polygon_file": utf8(&polygon.path)?,
                })
            }
        };
        extracts.push(entry);
    }
    let config = serde_json::json!({
        "directory": output_dir_str,
        "extracts": extracts,
    });
    let config_json = serde_json::to_string_pretty(&config)
        .map_err(|e| Error::Tool(format!("failed to encode multi-extract config: {e}")))?;

    Ok(MultiExtractConfig {
        json: config_json,
        outputs: regions
            .iter()
            .map(|named| output_dir.join(named.output_name()))
            .collect(),
        reads,
    })
}

/// Stable config filename shared by measured multi-extract and verification.
pub(crate) fn multi_extract_config_path(scratch_dir: &Path) -> PathBuf {
    scratch_dir.join("multi-extract-config.json")
}

fn utf8(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::tool(format!("non-UTF-8 path {}", path.display())))
}
#[allow(clippy::cast_precision_loss)]
pub(crate) fn strips(bbox: &Bbox, regions: usize) -> Result<Vec<Bbox>, Error> {
    if regions == 0 {
        return Err(Error::tool("multi-extract requires at least 1 region"));
    }
    let [west, south, east, north] = bbox.coordinates();
    let width = (east - west) / regions as f64;
    (0..regions)
        .map(|i| {
            let left = west + width * i as f64;
            let right = if i + 1 == regions {
                east
            } else {
                west + width * (i + 1) as f64
            };
            Ok(Bbox::parse(&format!("{left},{south},{right},{north}"))?)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn config_paths_and_effects_match_the_encoded_regions() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path().join("quoted-\"-directory");
        let polygon = scratch.path().join("city-\".geojson");
        let bbox = Bbox::parse("-1,2,3,4").unwrap();
        let strips = strips(&bbox, 3).unwrap();
        assert_eq!(strips[0].coordinates()[0], -1.0);
        assert_eq!(strips[2].coordinates()[2], 3.0);
        assert!(super::strips(&bbox, 0).is_err());
        let regions = multi_extract_regions(
            &strips,
            &[PolygonInput {
                name: "city".into(),
                path: polygon.clone(),
            }],
        );
        let config = multi_extract_config(&dir, &regions).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&config.json).unwrap();
        assert_eq!(doc["directory"], dir.to_str().unwrap());
        assert_eq!(
            doc["extracts"][3]["polygon_file"],
            polygon.to_str().unwrap()
        );
        for (entry, output) in doc["extracts"]
            .as_array()
            .unwrap()
            .iter()
            .zip(&config.outputs)
        {
            assert_eq!(dir.join(entry["output"].as_str().unwrap()), *output);
        }
        assert_eq!(config.reads.as_slice(), std::slice::from_ref(&polygon));
        let argv = PbfArgv::new("extract").extract_config(&dir.join("config.json"), config);
        assert_eq!(argv.writes().len(), 4);
        let (args, private) = argv.finish(&dir.join("launch"));
        assert!(args.contains(&"--staging-dir".into()));
        assert!(!args.contains(&"--tmp-dir".into()));
        assert_eq!(private, [dir.join("launch/staging")]);
        assert!(multi_extract_config(&dir, &[]).is_err());
    }
}
