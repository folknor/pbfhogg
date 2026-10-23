//! Streaming GeoJSON export.

mod geometry;
mod properties;
mod writer;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::commands::extract::Bbox;
use crate::owned::TypeFilter;
use crate::tag_expr::{Expression, parse_expressions, tag_matches};
use crate::{Element, ElementReader};

use geometry::{WayGeom, collect_coords, is_area_way, write_point, write_way_geometry};
use properties::{MetaView, write_properties};
use writer::FeatureWriter;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportFormat {
    #[default]
    GeoJsonSeq,
    GeoJson,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportTypes {
    #[default]
    All,
    NodesOnly,
    WaysOnly,
}

pub struct ExportOptions {
    format: ExportFormat,
    types: ExportTypes,
    expressions: Vec<Expression>,
    properties: Option<HashSet<String>>,
    bbox: Option<Bbox>,
    metadata: bool,
}

impl ExportOptions {
    pub fn new(
        format: ExportFormat,
        types: ExportTypes,
        expressions: &[String],
        properties: Option<Vec<String>>,
        bbox: Option<Bbox>,
        metadata: bool,
    ) -> crate::Result<Self> {
        let expressions = parse_expressions(expressions)?;
        Ok(Self {
            format,
            types,
            expressions,
            properties: properties.map(|keys| keys.into_iter().collect()),
            bbox,
            metadata,
        })
    }
}

#[derive(Default)]
pub struct ExportStats {
    pub nodes: u64,
    pub ways: u64,
    pub features: u64,
    pub skipped_untagged_nodes: u64,
    pub skipped_untagged_ways: u64,
    pub skipped_invalid_ways: u64,
}

impl ExportStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Exported {} features ({} nodes, {} ways); skipped {} untagged nodes, {} untagged ways, and {} invalid ways",
            self.features,
            self.nodes,
            self.ways,
            self.skipped_untagged_nodes,
            self.skipped_untagged_ways,
            self.skipped_invalid_ways
        )?;

        Ok(())
    }
}

fn admits_nodes(types: ExportTypes) -> bool {
    matches!(types, ExportTypes::All | ExportTypes::NodesOnly)
}

fn admits_ways(types: ExportTypes) -> bool {
    matches!(types, ExportTypes::All | ExportTypes::WaysOnly)
}

// All three call sites pass node_kind() or way_kind(), each exactly one kind.
// With that invariant, the implication tests below mean that the expression
// admits that kind; this is not a test for overlapping multi-kind filters.
// Export's filter is optional: no expressions means accept, whereas the PBF
// and OSC tags-filter predicates return false for an empty list. Clone the
// borrowed tag iterator per applicable expression rather than collecting tags.
// Parsing and individual tag semantics are shared through tag_expr.
fn matches_expressions<'a, T>(expressions: &[Expression], tags: &T, kind: &TypeFilter) -> bool
where
    T: Iterator<Item = (&'a str, &'a str)> + Clone,
{
    expressions.is_empty()
        || expressions.iter().any(|expression| {
            let admitted = (!kind.nodes || expression.type_filter.nodes)
                && (!kind.ways || expression.type_filter.ways)
                && (!kind.relations || expression.type_filter.relations);
            admitted
                && (*tags)
                    .clone()
                    .any(|(key, value)| tag_matches(&expression.matcher, key, value))
        })
}

fn node_kind() -> TypeFilter {
    TypeFilter {
        nodes: true,
        ways: false,
        relations: false,
    }
}

fn way_kind() -> TypeFilter {
    TypeFilter {
        nodes: false,
        ways: true,
        relations: false,
    }
}

/// Node and way source positions retain the PBF's nanodegree precision, and
/// the user's bbox is in degrees. Keep membership in that same precision:
/// truncating node coordinates to decimicrodegrees can put a node on the
/// opposite side of a bbox boundary from the same coordinate on a way.
fn way_hits_bbox(coords: &[(f64, f64)], bbox: &Bbox) -> bool {
    coords.iter().any(|&(lon, lat)| bbox.contains(lat, lon))
}

#[allow(clippy::too_many_lines)]
pub fn export<W: Write>(
    input: &Path,
    out: W,
    opts: &ExportOptions,
    host: &crate::host::Host,
) -> crate::Result<ExportStats> {
    host.cancel.check()?;
    let reader = ElementReader::open_in(input, false, host)?;
    if admits_ways(opts.types) && !reader.header().has_locations_on_ways() {
        return Err(crate::error::new_missing_feature(
            crate::HeaderBlock::LOCATIONS_ON_WAYS,
        ));
    }

    let mut writer = FeatureWriter::new(out, opts.format)?;
    let mut stats = ExportStats::default();
    let mut first_err = None;
    let mut geometry = String::new();
    let mut properties = String::new();
    let mut coord_scratch = String::new();
    let mut coords = Vec::new();

    let decode_result = reader.for_each(|element| {
        if first_err.is_some() {
            return;
        }
        let result = match element {
            Element::Node(node) if admits_nodes(opts.types) => {
                let tags = node.tags();
                if tags.clone().next().is_none() {
                    stats.skipped_untagged_nodes += 1;
                    return;
                }
                if opts
                    .bbox
                    .as_ref()
                    .is_some_and(|bbox| !bbox.contains(node.lat(), node.lon()))
                    || !matches_expressions(&opts.expressions, &tags, &node_kind())
                {
                    return;
                }
                write_point(&mut geometry, &mut coord_scratch, node.lon(), node.lat());
                let info = node.info();
                let meta = MetaView::from_info(&info);
                write_properties(&mut properties, node.id(), "node", tags, Some(&meta), opts)
                    .and_then(|()| {
                        writer.write_feature_geometry_props(&geometry, &properties)?;
                        stats.nodes += 1;
                        stats.features += 1;
                        Ok(())
                    })
            }
            Element::DenseNode(node) if admits_nodes(opts.types) => {
                let tags = node.tags();
                if tags.clone().next().is_none() {
                    stats.skipped_untagged_nodes += 1;
                    return;
                }
                if opts
                    .bbox
                    .as_ref()
                    .is_some_and(|bbox| !bbox.contains(node.lat(), node.lon()))
                    || !matches_expressions(&opts.expressions, &tags, &node_kind())
                {
                    return;
                }
                write_point(&mut geometry, &mut coord_scratch, node.lon(), node.lat());
                let meta = node.info().map(MetaView::from_dense);
                write_properties(
                    &mut properties,
                    node.id(),
                    "node",
                    tags,
                    meta.as_ref(),
                    opts,
                )
                .and_then(|()| {
                    writer.write_feature_geometry_props(&geometry, &properties)?;
                    stats.nodes += 1;
                    stats.features += 1;
                    Ok(())
                })
            }
            Element::Way(way) if admits_ways(opts.types) => {
                let tags = way.tags();
                if tags.clone().next().is_none() {
                    stats.skipped_untagged_ways += 1;
                    return;
                }
                if !matches_expressions(&opts.expressions, &tags, &way_kind()) {
                    return;
                }
                let is_area = is_area_way(&way);
                collect_coords(&way, &mut coords);
                if opts
                    .bbox
                    .as_ref()
                    .is_some_and(|bbox| !way_hits_bbox(&coords, bbox))
                {
                    return;
                }
                if write_way_geometry(&mut geometry, &mut coord_scratch, &coords, is_area)
                    == WayGeom::Invalid
                {
                    stats.skipped_invalid_ways += 1;
                    return;
                }
                let info = way.info();
                let meta = MetaView::from_info(&info);
                write_properties(&mut properties, way.id(), "way", tags, Some(&meta), opts)
                    .and_then(|()| {
                        writer.write_feature_geometry_props(&geometry, &properties)?;
                        stats.ways += 1;
                        stats.features += 1;
                        Ok(())
                    })
            }
            _ => return,
        };
        if let Err(error) = result {
            first_err = Some(error);
        }
    });

    // A write error recorded by the callback came first: the read carried on
    // past it, so its own error (a cancellation observed at a later blob
    // included) is the later one.
    if let Some(error) = first_err {
        return Err(error);
    }
    decode_result?;
    writer.finish()?;
    // The completion decision (the caller publishes, or has streamed to its
    // own writer).
    host.cancel.check()?;
    Ok(stats)
}
