use crate::read::columnar::{decimicro, dense_decimicro_coords};
use crate::read::input::InputSource;
use crate::read::project_ids::Span;
use crate::read::wire::WireInfo;

use crate::commands::require_indexdata;
use crate::error::Result;

const BLOCK_SIZE: usize = 128;

/// Bit-width histogram bucket boundaries.
const BUCKETS: &[(u32, u32)] = &[(0, 8), (9, 16), (17, 20), (21, 24), (25, 28), (29, 32)];

fn bucket_index(bits: u32) -> usize {
    match bits {
        0..=8 => 0,
        9..=16 => 1,
        17..=20 => 2,
        21..=24 => 3,
        25..=28 => 4,
        _ => 5,
    }
}

fn bits_needed(range: u32) -> u32 {
    if range == 0 {
        0
    } else {
        u32::BITS - range.leading_zeros()
    }
}

/// Statistics for one coordinate dimension (lat or lon).
pub struct CoordStats {
    /// Histogram: count of FOR blocks per bit-width bucket.
    pub bucket_counts: [u64; 6],
    /// Total number of complete or partial 128-value blocks analyzed.
    pub total_blocks: u64,
    /// Sum of bits_needed across all blocks (for weighted average).
    pub total_bits_weighted: u64,
}

impl CoordStats {
    fn new() -> Self {
        Self {
            bucket_counts: [0; 6],
            total_blocks: 0,
            total_bits_weighted: 0,
        }
    }

    fn record_block(&mut self, values: &[i32]) {
        let Some((&first, rest)) = values.split_first() else {
            return;
        };
        let (min, max) = rest
            .iter()
            .fold((first, first), |(min, max), &v| (min.min(v), max.max(v)));
        // The distance between two i32s always fits a u32.
        let range = max.abs_diff(min);
        let bits = bits_needed(range);
        self.bucket_counts[bucket_index(bits)] += 1;
        self.total_blocks += 1;
        self.total_bits_weighted += u64::from(bits);
    }

    pub fn avg_bits(&self) -> f64 {
        if self.total_blocks == 0 {
            0.0
        } else {
            self.total_bits_weighted as f64 / self.total_blocks as f64
        }
    }
}

/// Full report from `node_stats`.
pub struct NodeStatsReport {
    pub node_count: u64,
    pub min_lat: i32,
    pub max_lat: i32,
    pub min_lon: i32,
    pub max_lon: i32,
    pub lat_stats: CoordStats,
    pub lon_stats: CoordStats,
}

impl NodeStatsReport {
    /// Write the human-readable node statistics report to `out`.
    ///
    /// Rendered into a caller-supplied writer rather than the process's
    /// stdout so a write failure - the reader of a pipe going away - comes
    /// back as an error instead of a `println!` panic.
    ///
    /// # Errors
    ///
    /// Returns the first error `out` reports.
    pub fn write_report<W: std::io::Write>(&self, out: &mut W) -> std::io::Result<()> {
        writeln!(out, "Node count: {}", self.node_count)?;
        writeln!(out)?;
        writeln!(out, "Coordinate ranges (e7):")?;
        writeln!(out, "  lat: {} .. {}", self.min_lat, self.max_lat)?;
        writeln!(out, "  lon: {} .. {}", self.min_lon, self.max_lon)?;
        writeln!(out)?;

        writeln!(
            out,
            "FOR block bit-width distribution (block size = {BLOCK_SIZE}):"
        )?;
        writeln!(out)?;
        write_histogram(out, "lat", &self.lat_stats)?;
        writeln!(out)?;
        write_histogram(out, "lon", &self.lon_stats)?;
        writeln!(out)?;

        let lat_avg = self.lat_stats.avg_bits();
        let lon_avg = self.lon_stats.avg_bits();
        writeln!(out, "Weighted average bit-width:")?;
        writeln!(out, "  lat: {lat_avg:.2} bits")?;
        writeln!(out, "  lon: {lon_avg:.2} bits")?;
        writeln!(out)?;

        let num_blocks = self.lat_stats.total_blocks;
        // per block: avg_bits * 128 / 8 bytes for packed values + 4 bytes for min_value
        // two dimensions (lat + lon)
        let lat_bytes_per_block = lat_avg * BLOCK_SIZE as f64 / 8.0 + 4.0;
        let lon_bytes_per_block = lon_avg * BLOCK_SIZE as f64 / 8.0 + 4.0;
        let total_bytes = num_blocks as f64 * (lat_bytes_per_block + lon_bytes_per_block);
        let uncompressed_bytes = self.node_count as f64 * 8.0;

        writeln!(out, "Estimated compressed size:")?;
        writeln!(
            out,
            "  {:.2} GB ({} blocks x ({:.1} + {:.1}) bytes/block)",
            total_bytes / 1_073_741_824.0,
            num_blocks,
            lat_bytes_per_block,
            lon_bytes_per_block,
        )?;
        writeln!(
            out,
            "  vs {:.2} GB uncompressed ({} nodes x 8 bytes)",
            uncompressed_bytes / 1_073_741_824.0,
            self.node_count,
        )?;
        if uncompressed_bytes > 0.0 {
            writeln!(
                out,
                "  ratio: {:.1}%",
                total_bytes / uncompressed_bytes * 100.0
            )?;
        }
        Ok(())
    }
}

fn write_histogram(
    out: &mut impl std::io::Write,
    label: &str,
    stats: &CoordStats,
) -> std::io::Result<()> {
    writeln!(out, "  {label}:")?;
    for (i, &(lo, hi)) in BUCKETS.iter().enumerate() {
        let count = stats.bucket_counts[i];
        let pct = if stats.total_blocks > 0 {
            count as f64 / stats.total_blocks as f64 * 100.0
        } else {
            0.0
        };
        writeln!(
            out,
            "    {lo:>2}-{hi:<2} bits: {count:>10} blocks ({pct:>5.1}%)"
        )?;
    }
    Ok(())
}

/// Per-worker state for [`node_stats`]. Held across all blobs a worker
/// processes and merged into the global report at completion.
///
/// `lat_block` / `lon_block` are the in-flight 128-value windows that
/// feed [`CoordStats::record_block`]; they persist across blobs within a
/// worker and are flushed once at merge time. Cross-blob carry is kept
/// deliberately - the FOR block stat is about locality of consecutive
/// coordinate values, not blob-level grouping, and matching the
/// sequential behaviour of "only the last block is partial" keeps the
/// output closest to the pre-parallel path. Ordering is still different
/// in the parallel path (workers see blobs out of file order), so the
/// final block distribution is not byte-identical between runs but the
/// histogram shape is preserved.
struct WorkerAccum {
    node_count: u64,
    min_lat: i32,
    max_lat: i32,
    min_lon: i32,
    max_lon: i32,
    lat_stats: CoordStats,
    lon_stats: CoordStats,
    lat_block: Vec<i32>,
    lon_block: Vec<i32>,
}

impl WorkerAccum {
    fn new() -> Self {
        Self {
            node_count: 0,
            min_lat: i32::MAX,
            max_lat: i32::MIN,
            min_lon: i32::MAX,
            max_lon: i32::MIN,
            lat_stats: CoordStats::new(),
            lon_stats: CoordStats::new(),
            lat_block: Vec::with_capacity(BLOCK_SIZE),
            lon_block: Vec::with_capacity(BLOCK_SIZE),
        }
    }

    fn finalize(&mut self) {
        if !self.lat_block.is_empty() {
            self.lat_stats.record_block(&self.lat_block);
            self.lon_stats.record_block(&self.lon_block);
            self.lat_block.clear();
            self.lon_block.clear();
        }
    }

    /// Fold one node's coordinates (decimicrodegrees) in.
    #[inline]
    fn record(&mut self, lat_e7: i32, lon_e7: i32) {
        self.node_count += 1;
        self.min_lat = self.min_lat.min(lat_e7);
        self.max_lat = self.max_lat.max(lat_e7);
        self.min_lon = self.min_lon.min(lon_e7);
        self.max_lon = self.max_lon.max(lon_e7);

        self.lat_block.push(lat_e7);
        self.lon_block.push(lon_e7);

        if self.lat_block.len() == BLOCK_SIZE {
            self.lat_stats.record_block(&self.lat_block);
            self.lon_stats.record_block(&self.lon_block);
            self.lat_block.clear();
            self.lon_block.clear();
        }
    }
}

/// One worker's state: its accumulator and the scratch [`NodeCoords`]
/// records each blob into.
struct WorkerState {
    accum: WorkerAccum,
    coords: CoordScratch,
}

/// The node coordinates of a block, read through the block validator
/// (`BlockBuffer::project`) instead of a built block: a dense group's
/// validated `lat` and `lon` arrays are kept as positions and decoded once
/// the blob is accepted, where the element walk stepped `id`, `lat`, `lon`
/// and `keys_vals` per node; a plain node's coordinates, already parsed by
/// the validator, are scaled and copied.
///
/// Validation is block construction's own call, so a blob is refused here
/// exactly when building its block would refuse it, with the same error,
/// and a contradicted kind claim after it. What a hook records is
/// provisional (a later group, the string table or the claim may still
/// refuse the blob), so nothing reaches the accumulator before
/// [`CoordScratch::each`] runs on the accepted bytes.
///
/// Order is the element walk's: groups in order, and within a group its
/// dense nodes before its plain ones, although the validator hands the plain
/// ones over first; a group's plain coordinates wait in `group_plain` until
/// the group ends. The FOR-window statistic is about consecutive values, so
/// the order is part of the answer.
struct NodeCoords<'s> {
    scratch: &'s mut CoordScratch,
}

impl<'s> NodeCoords<'s> {
    /// The projection under a block's scalar fields, recording into
    /// `scratch`, which it clears.
    fn new(fields: &crate::read::wire::BlockFields, scratch: &'s mut CoordScratch) -> Self {
        scratch.runs.clear();
        scratch.plain.clear();
        scratch.group_plain.clear();
        scratch.granularity = fields.granularity;
        scratch.lat_offset = fields.lat_offset;
        scratch.lon_offset = fields.lon_offset;
        NodeCoords { scratch }
    }
}

/// One stretch of a blob's node coordinates, in element order.
enum CoordRun {
    /// A dense group's validated `lat` and `lon` arrays.
    Dense { lat: Span, lon: Span },
    /// The next `count` entries of [`CoordScratch::plain`].
    Plain { count: usize },
}

/// A worker's scratch for [`NodeCoords`], reused across blobs: the current
/// blob's runs, its plain nodes' coordinates, the current group's, and the
/// block's scalar fields the dense runs are scaled by.
#[derive(Default)]
struct CoordScratch {
    runs: Vec<CoordRun>,
    plain: Vec<(i32, i32)>,
    group_plain: Vec<(i32, i32)>,
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
}

impl crate::read::block::GroupProjection for NodeCoords<'_> {
    fn group_begin(&mut self) {
        self.scratch.group_plain.clear();
    }

    fn node(&mut self, node: &crate::read::wire::WireNode<'_>, _info: Option<&WireInfo>) {
        let s = &mut *self.scratch;
        let gran = i64::from(s.granularity);
        s.group_plain.push((
            decimicro(s.lat_offset, gran, node.lat),
            decimicro(s.lon_offset, gran, node.lon),
        ));
    }

    fn dense(
        &mut self,
        dense: &crate::read::wire::WireDenseNodes<'_>,
        _ids: usize,
        _info: Option<&crate::read::wire::WireDenseInfo<'_>>,
    ) {
        self.scratch.runs.push(CoordRun::Dense {
            lat: Span::of(dense.lat_data),
            lon: Span::of(dense.lon_data),
        });
    }

    fn group_end(&mut self, _kinds: crate::element_kind::KindSet) {
        let count = self.scratch.group_plain.len();
        if count > 0 {
            let CoordScratch {
                plain, group_plain, ..
            } = &mut *self.scratch;
            plain.append(group_plain);
            self.scratch.runs.push(CoordRun::Plain { count });
        }
    }
}

impl CoordScratch {
    /// Hand every recorded node's coordinates to `f` in element-walk order,
    /// the dense runs decoded from `bytes`, the accepted blob they point into.
    fn each(&self, bytes: &[u8], mut f: impl FnMut(i32, i32)) -> Result<()> {
        let mut plain = self.plain.iter();
        for run in &self.runs {
            match *run {
                CoordRun::Dense { lat, lon } => dense_decimicro_coords(
                    lat.in_blob(bytes)?,
                    lon.in_blob(bytes)?,
                    self.granularity,
                    self.lat_offset,
                    self.lon_offset,
                )
                .for_each(|(lat, lon)| f(lat, lon)),
                CoordRun::Plain { count } => plain
                    .by_ref()
                    .take(count)
                    .for_each(|&(lat, lon)| f(lat, lon)),
            }
        }
        Ok(())
    }
}

/// Analyze node coordinate statistics from a PBF file.
///
/// Streams through all node blobs in parallel (pread workers via
/// `parallel_project_accumulate`), each blob read through the
/// `NodeCoords` projection on the block validator rather than a built
/// block, collecting coordinate ranges and FOR block bit-width
/// distributions into per-worker accumulators that are merged at
/// completion. Per-worker state is bounded (`CoordStats` + two 128-entry
/// block buffers + scalar mins/maxes / counters, plus one blob's run list
/// and plain-node coordinates), so the accumulate pool's safety envelope
/// applies comfortably.
///
/// The input is read twice (the indexdata probe, then the schedule walk),
/// so it must be a regular file (`InvalidArgument`
/// otherwise) and is pinned for the run (see
/// [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin is used
/// as it is. Where a later open cannot reach the pinned file, an input
/// replaced mid-run fails with `InputReplaced`.
#[hotpath::measure]
pub fn node_stats<'a>(
    input: impl Into<InputSource<'a>>,
    direct_io: bool,
    force: bool,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Host,
) -> Result<NodeStatsReport> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation
        .decided(|| node_stats_inner(input, direct_io, force, parallelism, host))
}

fn node_stats_inner(
    input: InputSource<'_>,
    direct_io: bool,
    force: bool,
    parallelism: crate::host::Parallelism,
    host: &crate::host::Entered<'_>,
) -> Result<NodeStatsReport> {
    // Two opens, each its own open file description: the probe below, then
    // the schedule walk whose descriptor the classify pool preads. Both are
    // derived from this one pin, so a file renamed over the input between
    // them cannot have its indexdata probed in one file and its nodes read
    // from the other. Taken before either.
    let pin = input.pin(host)?;
    require_indexdata(
        &pin,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. Without it, way and relation blobs cannot \
         be skipped, so they are decompressed too.",
        host,
    )?;

    host.invocation.marker("NODESTATS_START");

    use crate::scan::classify::{KindClaims::Refuse, build_classify_schedule as schedule_of};
    let (schedule, shared_file) = schedule_of(&pin, Some(crate::ElementKind::Node), Refuse, host)?;

    // `None` keeps the classify pool's own host-derived sizing.
    let thread_override = parallelism.fixed();
    let mut global = WorkerAccum::new();

    crate::scan::classify::parallel_project_accumulate(
        &shared_file,
        &schedule,
        thread_override,
        || WorkerState {
            accum: WorkerAccum::new(),
            coords: CoordScratch::default(),
        },
        |buffer, WorkerState { accum, coords }| {
            let (NodeCoords { .. }, accepted) =
                buffer.project(|fields, _| NodeCoords::new(fields, coords))?;
            coords.each(accepted.bytes(), |lat, lon| accum.record(lat, lon))
        },
        |WorkerState {
             accum: mut worker, ..
         }| {
            worker.finalize();
            global.node_count += worker.node_count;
            global.min_lat = global.min_lat.min(worker.min_lat);
            global.max_lat = global.max_lat.max(worker.max_lat);
            global.min_lon = global.min_lon.min(worker.min_lon);
            global.max_lon = global.max_lon.max(worker.max_lon);
            // CoordStats merge is additive on every field.
            for (dst, src) in global
                .lat_stats
                .bucket_counts
                .iter_mut()
                .zip(worker.lat_stats.bucket_counts.iter())
            {
                *dst += src;
            }
            global.lat_stats.total_blocks += worker.lat_stats.total_blocks;
            global.lat_stats.total_bits_weighted += worker.lat_stats.total_bits_weighted;
            for (dst, src) in global
                .lon_stats
                .bucket_counts
                .iter_mut()
                .zip(worker.lon_stats.bucket_counts.iter())
            {
                *dst += src;
            }
            global.lon_stats.total_blocks += worker.lon_stats.total_blocks;
            global.lon_stats.total_bits_weighted += worker.lon_stats.total_bits_weighted;
        },
        host,
    )?;

    if global.node_count == 0 {
        global.min_lat = 0;
        global.max_lat = 0;
        global.min_lon = 0;
        global.max_lon = 0;
    }

    host.invocation.marker("NODESTATS_END");
    Ok(NodeStatsReport {
        node_count: global.node_count,
        min_lat: global.min_lat,
        max_lat: global.max_lat,
        min_lon: global.min_lon,
        max_lon: global.max_lon,
        lat_stats: global.lat_stats,
        lon_stats: global.lon_stats,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{CoordScratch, NodeCoords};
    use crate::Element;
    use crate::read::wire_fixtures::{
        Group, block, dense, field_varint, mutations, plain_node, projection_fixtures, zigzag,
    };

    /// The coordinates the element walk yields, in its order.
    fn walked(block: &crate::PrimitiveBlock) -> Vec<(i32, i32)> {
        block
            .elements_skip_metadata()
            .filter_map(|el| match &el {
                Element::DenseNode(dn) => Some((dn.decimicro_lat(), dn.decimicro_lon())),
                Element::Node(n) => Some((n.decimicro_lat(), n.decimicro_lon())),
                _ => None,
            })
            .collect()
    }

    /// The shared fixtures, plus blocks whose scalar fields make the scaling
    /// matter: a granularity other than 100, offsets on both axes, and
    /// values whose nanodegrees wrap `i64` and whose decimicrodegrees
    /// narrow past `i32`.
    #[allow(
        clippy::cast_sign_loss,
        reason = "protobuf int64 fields carry two's complement"
    )]
    fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
        let mut out = projection_fixtures();
        let scaled = |mut bytes: Vec<u8>, granularity: i64, lat: i64, lon: i64| {
            field_varint(&mut bytes, 17, granularity as u64);
            field_varint(&mut bytes, 19, lat as u64);
            field_varint(&mut bytes, 20, lon as u64);
            bytes
        };
        let mixed = block(
            &[""],
            &[
                Group {
                    nodes: vec![
                        plain_node(5, 100, -100, &[], None),
                        plain_node(2, i64::MAX / 3, i64::MIN / 5, &[], None),
                    ],
                    dense: Some(dense(
                        &[(10, 1, -1), (3, i64::MAX, i64::MIN), (-4, -7, 7)],
                        &[],
                    )),
                    ..Group::default()
                },
                Group {
                    dense: Some(dense(&[(11, 123_456_789, -987_654_321)], &[])),
                    ..Group::default()
                },
                Group {
                    nodes: vec![plain_node(1, -3, 3, &[], None)],
                    ..Group::default()
                },
            ],
        );
        out.push((
            "mixed groups, granularity 7, offsets",
            scaled(mixed.clone(), 7, -123_456_789, 987_654_321),
        ));
        out.push((
            "mixed groups, granularity 1000, wrapping offsets",
            scaled(mixed.clone(), 1000, i64::MAX - 5, i64::MIN + 5),
        ));
        out.push(("mixed groups, defaults", mixed));
        // A plain node's zigzag coordinate field written last: the parser
        // keeps the last value, as the accessors do.
        let mut node = plain_node(9, 1, 2, &[], None);
        field_varint(&mut node, 8, zigzag(-77));
        out.push((
            "a plain node with a repeated lat field",
            block(
                &[""],
                &[Group {
                    nodes: vec![node],
                    ..Group::default()
                }],
            ),
        ));
        out
    }

    /// For every fixture, every truncation and every probed byte value: the
    /// projection refuses exactly what block construction refuses, with the
    /// same message, and where it accepts it yields the element walk's node
    /// coordinates in the walk's order (groups in order, dense nodes before
    /// plain ones within a group). One worker's scratch serves every case,
    /// so nothing recorded for a refused blob leaks into the next.
    #[test]
    fn node_coords_refuse_as_construction_and_match_the_element_walk() {
        let mut scratch = CoordScratch::default();
        let (mut st, mut gr) = (Vec::new(), Vec::new());
        for (what, bytes) in fixtures() {
            assert!(
                crate::PrimitiveBlock::from_vec(bytes.clone()).is_ok(),
                "{what}: fixture must be valid"
            );
            let cases =
                std::iter::once(("as built".to_owned(), bytes.clone())).chain(mutations(&bytes));
            for (how, input) in cases {
                let oracle = crate::PrimitiveBlock::from_vec(input.clone());
                let buffer =
                    crate::read::block::BlockBuffer::new(input, None, &mut st, &mut gr, None);
                let projected =
                    match buffer.project(|fields, _| NodeCoords::new(fields, &mut scratch)) {
                        Ok((NodeCoords { .. }, accepted)) => {
                            let mut coords = Vec::new();
                            scratch
                                .each(accepted.bytes(), |lat, lon| coords.push((lat, lon)))
                                .map(|()| coords)
                        }
                        Err(e) => Err(e),
                    };
                match (&oracle, projected) {
                    (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "{what}, {how}"),
                    (Ok(block), Ok(coords)) => assert_eq!(coords, walked(block), "{what}, {how}"),
                    (oracle, got) => panic!(
                        "{what}, {how}: construction accepted {}, projection accepted {}",
                        oracle.is_ok(),
                        got.is_ok()
                    ),
                }
            }
        }
    }

    /// A node blob claiming ways is refused after validation with the
    /// block path's error, and its coordinates never reach the caller.
    #[test]
    fn a_contradicted_claim_is_refused_like_construction() {
        let (_, bytes) = fixtures().swap_remove(0);
        let claim = Some(crate::ElementKind::Way);
        let (mut st, mut gr) = (Vec::new(), Vec::new());
        let mut scratch = CoordScratch::default();
        let projected =
            crate::read::block::BlockBuffer::new(bytes.clone(), None, &mut st, &mut gr, claim)
                .project(|fields, _| NodeCoords::new(fields, &mut scratch))
                .map(drop)
                .expect_err("a node blob claiming ways");
        let built = crate::read::block::BlockBuffer::new(bytes, None, &mut st, &mut gr, claim)
            .block()
            .map(drop)
            .expect_err("the block path refuses it");
        assert!(projected.kind_contradiction().is_some(), "{projected}");
        assert_eq!(projected.to_string(), built.to_string());
    }
}
