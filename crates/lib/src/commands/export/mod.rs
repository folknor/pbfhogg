//! Streaming GeoJSON export.
//!
//! The decode workers format each data block's features into a chunk of
//! complete feature bodies; the ordered consumer on the calling thread
//! writes them in file order and stops the read at the first failure.

mod geometry;
mod properties;
mod writer;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use crate::commands::extract::Bbox;
use crate::owned::TypeFilter;
use crate::read::pipeline::{DecodeTransform, Decoder};
use crate::tag_expr::{Expression, parse_expressions, tag_matches};
use crate::tunables::{EXPORT_CONSUMER_BATCH_BYTES, EXPORT_WORKER_CHUNK_CAP};
use crate::{Element, ElementKind, ElementReader, PrimitiveBlock};

use geometry::{Ordinate, Positions, is_area_way, way_shape, write_point, write_way};
use properties::{MetaClaims, SeenKeys, Stop, reserve, write_properties};
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
    pub skipped_invalid_nodes: u64,
    pub skipped_invalid_ways: u64,
}

impl ExportStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Exported {} features ({} nodes, {} ways); skipped {} untagged nodes, {} untagged ways, {} invalid nodes, and {} invalid ways",
            self.features,
            self.nodes,
            self.ways,
            self.skipped_untagged_nodes,
            self.skipped_untagged_ways,
            self.skipped_invalid_nodes,
            self.skipped_invalid_ways
        )?;

        Ok(())
    }

    fn add(&mut self, other: &Self) {
        self.nodes += other.nodes;
        self.ways += other.ways;
        self.features += other.features;
        self.skipped_untagged_nodes += other.skipped_untagged_nodes;
        self.skipped_untagged_ways += other.skipped_untagged_ways;
        self.skipped_invalid_nodes += other.skipped_invalid_nodes;
        self.skipped_invalid_ways += other.skipped_invalid_ways;
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

/// Node and way positions reach here as f64 degrees, and both go through the
/// one point test, `Bbox::contains`, which rounds the point to its nearest
/// decimicrodegree and compares it with the edges quantized as `extract
/// --bbox` quantizes them, inclusively. A node and the same
/// coordinate on a way therefore always land on the same side of the edge.
fn way_hits_bbox(way: &crate::Way<'_>, bbox: &Bbox) -> bool {
    way.node_locations()
        .any(|location| bbox.contains(location.lat(), location.lon()))
}

/// One block's formatted features.
#[derive(Default)]
struct Chunk {
    /// Every complete feature body, back to back, without separators.
    bytes: Vec<u8>,
    /// Byte end of each feature body in `bytes`.
    ends: Vec<usize>,
    stats: ExportStats,
    /// The refusal that ended the block, after its complete features: it is
    /// returned once they are written, as the serial walk wrote them before
    /// reaching the refused element. Carried in the result, not as the
    /// read's own error, so the engine does not relocate it at the blob.
    error: Option<crate::Error>,
}

impl Chunk {
    fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
        self.stats = ExportStats::default();
    }
}

/// What a decode worker hands the ordered consumer for one data block.
enum Formatted {
    Chunk(Chunk),
    /// Its text would pass [`EXPORT_WORKER_CHUNK_CAP`]: the consumer formats
    /// it again and writes it a batch at a time. The worker's own bytes and
    /// counts are discarded, so the block is counted once.
    Oversize(PrimitiveBlock),
}

/// The decode-worker side of export: skips a data blob whose indexdata
/// claims a kind export does not emit under `--type` (relations always),
/// and formats every other data block.
///
/// The skip is made here, not through a `BlobFilter`: a filter makes the
/// read note an unindexed input, and export proceeds without indexdata
/// silently. Expression and tag-key pruning are not made at all, though the
/// expressions and tagdata would allow them: the summary counts untagged
/// elements of every selected kind before any expression is applied, and a
/// blob skipped on its key set would drop them from it.
struct ExportBlocks<'o> {
    opts: &'o ExportOptions,
    cap: usize,
}

impl DecodeTransform for ExportBlocks<'_> {
    type Out = Formatted;
    const PARSE_INDEXDATA: bool = true;

    fn apply(
        &self,
        blob: &crate::blob::Blob,
        decode: Decoder<'_>,
    ) -> Option<crate::Result<Formatted>> {
        if blob.get_type() != crate::blob::BlobType::OsmData {
            return None;
        }
        if let Some(index) = blob.index()
            && !admits_kind(self.opts.types, index.kind)
        {
            return None;
        }
        Some(decode.block().map(|block| {
            let mut chunk = Chunk::default();
            let mut positions = Positions::default();
            match format_block(
                &block,
                self.opts,
                &mut positions,
                &mut chunk,
                self.cap,
                &mut |_| true,
            ) {
                Ok(()) => Formatted::Chunk(chunk),
                Err(Overflow) => Formatted::Oversize(block),
            }
        }))
    }
}

fn admits_kind(types: ExportTypes, kind: ElementKind) -> bool {
    match kind {
        ElementKind::Node => admits_nodes(types),
        ElementKind::Way => admits_ways(types),
        ElementKind::Relation => false,
    }
}

/// The formatting buffer's cap was reached.
struct Overflow;

/// Formats every element of `block` into `chunk`, appending only while the
/// chunk stays within `cap`. After each complete feature, `drained` may
/// write the chunk's complete features and clear it; it returns `false` to
/// stop the block. A refusal truncates the chunk to its complete features
/// and is recorded in `chunk.error`.
fn format_block(
    block: &PrimitiveBlock,
    opts: &ExportOptions,
    positions: &mut Positions,
    chunk: &mut Chunk,
    cap: usize,
    drained: &mut dyn FnMut(&mut Chunk) -> bool,
) -> Result<(), Overflow> {
    // Dense metadata is decoded only when printed; the block was validated
    // whole either way.
    let elements = if opts.metadata {
        block.elements()
    } else {
        block.elements_skip_metadata()
    };
    let mut seen = SeenKeys::default();
    for element in elements {
        let start = chunk.bytes.len();
        match format_element(&element, opts, positions, &mut seen, chunk, cap) {
            Ok(false) => continue,
            Ok(true) => {
                chunk.ends.push(chunk.bytes.len());
                if !drained(chunk) {
                    return Ok(());
                }
            }
            Err(Stop::Overflow) => return Err(Overflow),
            Err(Stop::Refused(error)) => {
                chunk.bytes.truncate(start);
                chunk.error = Some(error);
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Selects and formats one element, counting it. `Ok(true)` when a feature
/// body was appended.
fn format_element<'a>(
    element: &Element<'a>,
    opts: &ExportOptions,
    positions: &mut Positions,
    seen: &mut SeenKeys<'a>,
    chunk: &mut Chunk,
    cap: usize,
) -> Result<bool, Stop> {
    let mut feature = Feature {
        opts,
        positions,
        seen,
        chunk,
        cap,
    };
    match element {
        Element::Node(node) if admits_nodes(opts.types) => {
            let point = Point {
                lon: Ordinate {
                    nano: node.nano_lon_exact(),
                    deg: node.lon(),
                },
                lat: Ordinate {
                    nano: node.nano_lat_exact(),
                    deg: node.lat(),
                },
            };
            feature.node(node.id(), node.tags(), point, || {
                Some(MetaClaims::from_info(&node.info()))
            })
        }
        Element::DenseNode(node) if admits_nodes(opts.types) => {
            let point = Point {
                lon: Ordinate {
                    nano: node.nano_lon_exact(),
                    deg: node.lon(),
                },
                lat: Ordinate {
                    nano: node.nano_lat_exact(),
                    deg: node.lat(),
                },
            };
            feature.node(node.id(), node.tags(), point, || {
                node.info().map(crate::DenseNodeInfo::claims)
            })
        }
        Element::Way(way) if admits_ways(opts.types) => feature.way(way),
        _ => Ok(false),
    }
}

/// A node's position, as its accessors report it.
#[derive(Clone, Copy)]
struct Point {
    lon: Ordinate,
    lat: Ordinate,
}

/// One element's formatting, over the block's shared state.
struct Feature<'f, 'a> {
    opts: &'f ExportOptions,
    positions: &'f mut Positions,
    seen: &'f mut SeenKeys<'a>,
    chunk: &'f mut Chunk,
    cap: usize,
}

impl<'a> Feature<'_, 'a> {
    /// A tagged node selected by `--bbox` and the expressions becomes a
    /// Point; an untagged one, or one outside the geographic domain, is
    /// counted. `meta` is read only under `--metadata`.
    fn node<T>(
        &mut self,
        id: i64,
        tags: T,
        point: Point,
        meta: impl FnOnce() -> Option<MetaClaims<'a>>,
    ) -> Result<bool, Stop>
    where
        T: Iterator<Item = (&'a str, &'a str)> + Clone,
    {
        let opts = self.opts;
        if tags.clone().next().is_none() {
            self.chunk.stats.skipped_untagged_nodes += 1;
            return Ok(false);
        }
        if opts
            .bbox
            .as_ref()
            .is_some_and(|bbox| !bbox.contains(point.lat.deg, point.lon.deg))
            || !matches_expressions(&opts.expressions, &tags, &node_kind())
        {
            return Ok(false);
        }
        if !self.positions.fill_point(point.lon, point.lat) {
            self.chunk.stats.skipped_invalid_nodes += 1;
            return Ok(false);
        }
        let meta = if opts.metadata { meta() } else { None };
        let out = &mut self.chunk.bytes;
        begin_feature(out, self.positions, self.cap)?;
        write_point(out, self.positions);
        end_feature(
            out,
            self.seen,
            id,
            "node",
            tags,
            meta.as_ref(),
            opts,
            self.cap,
        )?;
        self.chunk.stats.nodes += 1;
        self.chunk.stats.features += 1;
        Ok(true)
    }

    /// A tagged way selected by the expressions and `--bbox` becomes a
    /// LineString or Polygon; an untagged one, or one with invalid
    /// geometry, is counted.
    fn way(&mut self, way: &crate::Way<'a>) -> Result<bool, Stop> {
        let opts = self.opts;
        let tags = way.tags();
        if tags.clone().next().is_none() {
            self.chunk.stats.skipped_untagged_ways += 1;
            return Ok(false);
        }
        if !matches_expressions(&opts.expressions, &tags, &way_kind()) {
            return Ok(false);
        }
        let is_area = is_area_way(way);
        if opts
            .bbox
            .as_ref()
            .is_some_and(|bbox| !way_hits_bbox(way, bbox))
        {
            return Ok(false);
        }
        let shape = if self.positions.fill_way(way) {
            way_shape(self.positions, is_area)
        } else {
            None
        };
        let Some(shape) = shape else {
            self.chunk.stats.skipped_invalid_ways += 1;
            return Ok(false);
        };
        let meta = opts.metadata.then(|| MetaClaims::from_info(&way.info()));
        let out = &mut self.chunk.bytes;
        begin_feature(out, self.positions, self.cap)?;
        write_way(out, self.positions, shape);
        end_feature(
            out,
            self.seen,
            way.id(),
            "way",
            tags,
            meta.as_ref(),
            opts,
            self.cap,
        )?;
        self.chunk.stats.ways += 1;
        self.chunk.stats.features += 1;
        Ok(true)
    }
}

/// Starts a feature body whose geometry is `positions`, once the chunk has
/// room for that geometry.
fn begin_feature(out: &mut Vec<u8>, positions: &Positions, cap: usize) -> Result<(), Stop> {
    reserve(out, positions.bound(), cap)?;
    out.extend_from_slice(b"{\"type\":\"Feature\",\"geometry\":");
    Ok(())
}

/// Ends a feature body with its properties.
#[allow(clippy::too_many_arguments)]
fn end_feature<'a, T>(
    out: &mut Vec<u8>,
    seen: &mut SeenKeys<'a>,
    id: i64,
    object_type: &str,
    tags: T,
    meta: Option<&MetaClaims<'a>>,
    opts: &ExportOptions,
    cap: usize,
) -> Result<(), Stop>
where
    T: Iterator<Item = (&'a str, &'a str)>,
{
    out.extend_from_slice(b",\"properties\":");
    write_properties(out, seen, id, object_type, tags, meta, opts, cap)?;
    out.push(b'}');
    Ok(())
}

pub fn export<W: Write>(
    input: &Path,
    out: W,
    opts: &ExportOptions,
    host: &crate::host::Host,
) -> crate::Result<ExportStats> {
    export_capped(input, out, opts, host, EXPORT_WORKER_CHUNK_CAP)
}

/// [`export`] with the worker's chunk cap as a parameter, so a test can
/// send every block down the consumer's path.
fn export_capped<W: Write>(
    input: &Path,
    out: W,
    opts: &ExportOptions,
    host: &crate::host::Host,
    cap: usize,
) -> crate::Result<ExportStats> {
    let host = &host.enter()?;
    host.invocation.check()?;
    let reader = ElementReader::open_in(input, false, host)?;
    if admits_ways(opts.types) && !reader.header().has_locations_on_ways() {
        return Err(crate::error::new_missing_feature(
            crate::HeaderBlock::LOCATIONS_ON_WAYS,
        ));
    }

    // A decoded block holding a kind its indexdata does not claim is
    // refused: the blobs `ExportBlocks` skips are skipped on that same
    // claim.
    let reader = reader.refusing_kind_claims();

    let mut writer = FeatureWriter::new(out, opts.format)?;
    let mut stats = ExportStats::default();
    let mut positions = Positions::default();
    let mut oversize = 0_u64;

    // Everything reaches this consumer in file order, a worker's refusal
    // included (carried in its chunk, after the features before it), so the
    // first failure in file order wins, as in a serial walk. A write error
    // ends the read here, and the engine stops reading behind it, finishing
    // only work already admitted and joining its threads. Cancellation is
    // the engine's rule: checked here before each wait and on each arrival.
    reader.for_each_fused_blob(ExportBlocks { opts, cap }, |formatted| match formatted {
        Formatted::Chunk(chunk) => drain(&mut writer, &mut stats, chunk),
        Formatted::Oversize(block) => {
            oversize += 1;
            let mut chunk = Chunk::default();
            let mut failed = None;
            let fits = format_block(
                &block,
                opts,
                &mut positions,
                &mut chunk,
                usize::MAX,
                &mut |chunk| {
                    if chunk.bytes.len() < EXPORT_CONSUMER_BATCH_BYTES {
                        return true;
                    }
                    match write_chunk(&mut writer, chunk) {
                        Ok(()) => {
                            stats.add(&chunk.stats);
                            chunk.clear();
                            true
                        }
                        Err(error) => {
                            failed = Some(error);
                            false
                        }
                    }
                },
            );
            if let Some(error) = failed {
                return Err(error.into());
            }
            if fits.is_err() {
                return Err(crate::Error::internal(
                    "export overflowed a buffer with no cap",
                ));
            }
            drain(&mut writer, &mut stats, chunk)
        }
    })?;
    // Blocks formatted on the consumer, past the worker's cap.
    host.invocation.counter("export_oversize_blocks", oversize);
    writer.finish()?;
    // The completion decision (the caller publishes, or has streamed to its
    // own writer).
    host.invocation.check()?;
    Ok(stats)
}

/// Writes a chunk's complete features, one at a time.
fn write_chunk<W: Write>(writer: &mut FeatureWriter<W>, chunk: &Chunk) -> std::io::Result<()> {
    let mut start = 0;
    for &end in &chunk.ends {
        writer.write_feature(&chunk.bytes[start..end])?;
        start = end;
    }
    Ok(())
}

/// Writes a chunk's features, then counts it, then returns its refusal.
fn drain<W: Write>(
    writer: &mut FeatureWriter<W>,
    stats: &mut ExportStats,
    chunk: Chunk,
) -> crate::Result<()> {
    write_chunk(writer, &chunk)?;
    stats.add(&chunk.stats);
    chunk.error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
    use crate::writer::{Compression, PbfWriter};

    const META: Metadata<'static> = Metadata {
        version: 3,
        timestamp: 1_700_000_000,
        changeset: 9,
        uid: 4,
        user: "mapper",
        visible: true,
    };

    /// Node blocks then a way block, with untagged elements, an invalid
    /// node, a long value shared by many elements, a polygon and a line.
    fn write_fixture(path: &Path) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
        let header = HeaderBuilder::new()
            .sorted()
            .optional_feature(crate::HeaderBlock::LOCATIONS_ON_WAYS)
            .build()
            .expect("header");
        writer.write_header(&header).expect("write header");
        let long = "v\u{1}\"".repeat(2_000);
        let mut bb = BlockBuilder::new();
        for blob in 0..3_i64 {
            for i in 0..50_i64 {
                let id = blob * 100 + i + 1;
                let lat = i32::try_from(id * 1_000).expect("lat");
                match i % 5 {
                    0 => bb.add_node(id, lat, -lat, std::iter::empty(), Some(&META)),
                    1 => bb.add_node(id, 950_000_000, 0, [("name", "pole")], Some(&META)),
                    2 => bb.add_node(id, lat, lat, [("name", long.as_str())], Some(&META)),
                    _ => bb.add_node(
                        id,
                        lat,
                        lat + 7,
                        [("amenity", "bench"), ("name", "x"), ("name", "dup")],
                        Some(&META),
                    ),
                }
            }
            let bytes = bb.take().expect("take").expect("block");
            writer.write_primitive_block(bytes).expect("write block");
        }
        let ring = [(0, 0), (0, 10_000_000), (10_000_000, 0), (0, 0)];
        bb.add_way_with_locations(1, [("building", "yes")], &[1, 2, 3, 1], &ring, Some(&META));
        bb.add_way_with_locations(
            2,
            [("highway", "path")],
            &[1, 2],
            &[(5, 6), (7, 8)],
            Some(&META),
        );
        bb.add_way_with_locations(3, std::iter::empty(), &[1, 2], &[(5, 6), (7, 8)], None);
        let bytes = bb.take().expect("take").expect("block");
        writer.write_primitive_block(bytes).expect("write block");
        writer.flush().expect("flush");
    }

    fn options(format: ExportFormat, metadata: bool) -> ExportOptions {
        ExportOptions::new(format, ExportTypes::All, &[], None, None, metadata).expect("options")
    }

    fn run(input: &Path, opts: &ExportOptions, cap: usize) -> (Vec<u8>, [u64; 7]) {
        let mut out = Vec::new();
        let stats = export_capped(input, &mut out, opts, &crate::host::Host::for_tests(), cap)
            .expect("export");
        let counts = [
            stats.nodes,
            stats.ways,
            stats.features,
            stats.skipped_untagged_nodes,
            stats.skipped_untagged_ways,
            stats.skipped_invalid_nodes,
            stats.skipped_invalid_ways,
        ];
        (out, counts)
    }

    /// A block handed back to the consumer, at every cap from "every block"
    /// to "none", is written byte for byte as a worker would have written
    /// it, with the same counts.
    #[test]
    fn oversize_blocks_match_worker_chunks() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("fixture.osm.pbf");
        write_fixture(&input);
        for format in [ExportFormat::GeoJsonSeq, ExportFormat::GeoJson] {
            for metadata in [false, true] {
                let opts = options(format, metadata);
                let (want, want_counts) = run(&input, &opts, usize::MAX);
                assert_eq!(want_counts, [90, 2, 92, 30, 1, 30, 0]);
                for cap in [0, 1_000, 20_000, 100_000] {
                    let (got, counts) = run(&input, &opts, cap);
                    assert_eq!(counts, want_counts, "cap {cap}");
                    assert!(got == want, "cap {cap}: output differs");
                }
            }
        }
    }

    /// A writer that fails once it has accepted `budget` calls.
    struct FailAfter {
        budget: usize,
        out: Vec<u8>,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.budget == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            self.budget -= 1;
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A refusal later in a block comes after the block's earlier features:
    /// they are written first, and a write failure on one of them wins.
    #[test]
    fn refusal_follows_the_features_before_it() {
        let dir = crate::path_guard::tests::scratch();
        let input = dir.path().join("refused.osm.pbf");
        let file = std::fs::File::create(&input).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("write header");
        let mut bb = BlockBuilder::new();
        bb.add_node(1, 0, 0, [("name", "good")], Some(&META));
        let overflow = Metadata {
            timestamp: (1_i64 << 61) + 1,
            ..META
        };
        bb.add_node(2, 0, 0, [("name", "bad")], Some(&overflow));
        let bytes = bb.take().expect("take").expect("block");
        writer.write_primitive_block(bytes).expect("write block");
        writer.flush().expect("flush");

        let opts = ExportOptions::new(
            ExportFormat::GeoJsonSeq,
            ExportTypes::NodesOnly,
            &[],
            None,
            None,
            true,
        )
        .expect("options");
        for cap in [usize::MAX, 0] {
            let mut sink = FailAfter {
                budget: usize::MAX,
                out: Vec::new(),
            };
            let error = export_capped(
                &input,
                &mut sink,
                &opts,
                &crate::host::Host::for_tests(),
                cap,
            )
            .err()
            .expect("refused");
            assert!(error.to_string().contains("node 2"), "{error}");
            let text = String::from_utf8(sink.out).expect("UTF-8");
            assert_eq!(text.lines().count(), 1, "{text}");
            assert!(text.contains("\"good\""), "{text}");

            let mut sink = FailAfter {
                budget: 0,
                out: Vec::new(),
            };
            let error = export_capped(
                &input,
                &mut sink,
                &opts,
                &crate::host::Host::for_tests(),
                cap,
            )
            .err()
            .expect("write failure");
            assert!(
                matches!(error.kind(), crate::ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::BrokenPipe),
                "{error}"
            );
        }
    }
}
