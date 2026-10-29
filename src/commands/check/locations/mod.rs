//! `check --locations`: verify a LocationsOnWays file's inline way-node
//! coordinates against the node records in the same file.
//!
//! # What it establishes
//!
//! A file whose header declares `LocationsOnWays` claims that every way
//! carries, for each node it references, that node's location. This check
//! compares every such inline coordinate with the node record of the same ID
//! in the same file, and reports each disagreement. It also reports the
//! structural ways the claim can fail without a node to compare against: a
//! way with references but no coordinate arrays, and an inline coordinate
//! outside the geographic domain (which includes libosmium's undefined
//! location, `2147483647` on both axes, written for a node it could not
//! resolve).
//!
//! # References to nodes the file does not contain
//!
//! `add-locations-to-ways` drops untagged nodes that no relation references
//! unless told to keep them, so on its default output most references have no
//! node record. Such a reference cannot be verified, only counted, and it is
//! **not a violation**: the file's claim is about the coordinates it carries,
//! and nothing in `LocationsOnWays` requires the referenced node to be present.
//! The report counts these references (and, among them, those at exactly
//! `(0, 0)`, the placeholder `--ignore-missing-nodes` writes), so a pass on a
//! file that verified little says so.
//!
//! With `agreement` set the check additionally requires every inline copy of
//! such a node's coordinate to be the same, and reports each absent node whose
//! references disagree. That establishes consistency, not freshness: if every
//! way referencing an omitted node carries the same stale coordinate, nothing
//! in the file can show it. It is opt-in because it needs a slot for every
//! referenced node rather than every present one, which on a default
//! `add-locations-to-ways` output is the difference between a few hundred
//! million slots and every referenced node of the planet.
//!
//! # Comparison
//!
//! Coordinates are compared as decimicrodegrees (e7), the unit every
//! coordinate store in this crate and the OSM data model use, after applying
//! each block's `granularity`, `lat_offset` and `lon_offset` exactly (see
//! `scan`); two values that agree to the decimicrodegree are equal.
//!
//! # Passes
//!
//! 1. Every blob not claiming ways (node, relation and unindexed blobs) is
//!    decoded once: validated as the reader validates a block, its node
//!    records counted and their IDs collected into the key set (the node IDs
//!    that get a slot), and the element kinds and node-ID range it actually
//!    holds recorded. Later passes route these blobs on what this pass saw,
//!    not on their indexdata.
//! 2. With `agreement`, every way blob's references join the key set too.
//! 3. The key set is ranked and its rank space cut into partitions whose slot
//!    store fits the budget (`store`). Per partition, the blobs that hold
//!    nodes in its ID interval are decoded and their records joined into the
//!    store, then every way blob is decoded and each reference the partition
//!    owns is checked. The first partition also makes the checks that do not
//!    depend on the partition, and validates every blob claiming ways, which
//!    pass 1 did not read; one whose groups hold another kind is refused,
//!    since its nodes were not collected.
//!
//! # Memory
//!
//! The store budget bounds one partition's slot store and nothing else. On
//! top of it: the key set (a bit set over the node-ID space, about 1.7 GB
//! for a planet's node IDs) and its rank index, which follow the number of
//! distinct IDs; each worker's decompressed blob, its validation copy and a
//! fixed ID batch (`CHECK_LOCATIONS_KEY_BATCH`); and the retained violation
//! details, at most `max_errors` per blob in flight and in total, but every
//! violation when `max_errors` is zero. Way scanning streams one reference
//! at a time, so a long way costs no worker memory. No scratch disk is used.

mod scan;
mod store;

use std::sync::Mutex;

use crate::ElementKind;
use crate::error::{Error, Result};
use crate::idset::{IdSet, RankedIdSet};
use crate::read::input::{InputFile, InputSource, PinnedInput};
use crate::scan::classify::{ScheduledBlob, parallel_scan_blobs_raw};
use crate::tunables::{
    CHECK_LOCATIONS_FALLBACK_BUDGET_BYTES, CHECK_LOCATIONS_HEADROOM_BYTES,
    CHECK_LOCATIONS_KEY_BATCH, CHECK_LOCATIONS_KEY_SHARDS, CHECK_LOCATIONS_MIN_BUDGET_BYTES,
    CHECK_LOCATIONS_PARTITION_WARN, CHECK_LOCATIONS_STORE_BUDGET_PCT,
};

use self::scan::{
    Coord, Groups, Kinds, ValidateScratch, WayEvent, scan_nodes, scan_ways, validate_block,
};
use self::store::{Plan, SlotState, Store, in_partition, range_meets, slots_for_budget};

/// Options for [`check_locations`].
#[derive(Debug, Clone, Copy)]
pub struct CheckLocationsOptions {
    /// Options shared with the other checks. `direct_io` is accepted and has
    /// no effect: the parallel readers pread unaligned blob ranges.
    pub check: super::CheckOptions,
    /// Also require the inline coordinates of every node the file does not
    /// contain to agree across all references to it. See the module docs for
    /// what it establishes and what it costs.
    pub agreement: bool,
    /// Bytes one partition's coordinate store may take. `None` derives it
    /// from the host's available memory. The store costs about 8.25 bytes per
    /// node ID with a slot; a smaller budget means more passes over the way
    /// blobs, never a different answer. It bounds the store only; see the
    /// module docs for the rest of the run's memory.
    pub store_budget_bytes: Option<u64>,
}

/// An e7 coordinate as `(latitude, longitude)`.
pub type E7 = (i32, i32);

/// One way a file's inline locations are wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocationViolationKind {
    /// A way's inline coordinate for a node differs from that node's record.
    Mismatch {
        way_id: i64,
        node_id: i64,
        /// Position of the reference within the way, from 0.
        position: u64,
        inline: E7,
        node: E7,
    },
    /// A way's inline coordinate lies outside the geographic domain.
    /// libosmium's undefined location is one.
    InvalidInline {
        way_id: i64,
        node_id: i64,
        position: u64,
        /// Exact value in nanodegrees.
        lat_nano: i128,
        lon_nano: i128,
    },
    /// A way has node references but no coordinate arrays.
    WayWithoutLocations { way_id: i64 },
    /// Two or more records of one node carry different coordinates (and none
    /// an invalid one), so the node has no single location to compare with.
    ConflictingNode { node_id: i64 },
    /// A node record's coordinate lies outside the geographic domain.
    InvalidNodeRecord {
        node_id: i64,
        lat_nano: i128,
        lon_nano: i128,
    },
    /// Agreement mode: the file has no record of this node and the ways
    /// referencing it carry at least two different coordinates for it.
    DisagreeingRefs { node_id: i64 },
}

/// One violation, located where the file allows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationViolation {
    /// Ordinal of the blob holding the element, for the element-level
    /// violations; `None` for the node-level ones found by ID.
    pub blob_ordinal: Option<u64>,
    pub kind: LocationViolationKind,
    /// Total order the retained details are chosen and listed by: category,
    /// then position in the file or node ID. Independent of worker timing and
    /// of how many partitions the run used.
    key: (u8, u64, u64, u64),
}

fn fmt_e7(f: &mut std::fmt::Formatter<'_>, v: i32) -> std::fmt::Result {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    write!(f, "{sign}{}.{:07}", a / 10_000_000, a % 10_000_000)
}

fn fmt_pair(f: &mut std::fmt::Formatter<'_>, (lat, lon): E7) -> std::fmt::Result {
    fmt_e7(f, lat)?;
    f.write_str(",")?;
    fmt_e7(f, lon)
}

impl std::fmt::Display for LocationViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            LocationViolationKind::Mismatch {
                way_id,
                node_id,
                position,
                inline,
                node,
            } => {
                write!(f, "w{way_id} ref {position} (n{node_id}): inline ")?;
                fmt_pair(f, *inline)?;
                f.write_str(", node record ")?;
                fmt_pair(f, *node)
            }
            LocationViolationKind::InvalidInline {
                way_id,
                node_id,
                position,
                lat_nano,
                lon_nano,
            } => write!(
                f,
                "w{way_id} ref {position} (n{node_id}): inline coordinate outside \
                 the geographic domain (lat {lat_nano}, lon {lon_nano} nanodegrees)"
            ),
            LocationViolationKind::WayWithoutLocations { way_id } => {
                write!(f, "w{way_id}: node references without inline coordinates")
            }
            LocationViolationKind::ConflictingNode { node_id } => {
                write!(f, "n{node_id}: node records disagree about its coordinate")
            }
            LocationViolationKind::InvalidNodeRecord {
                node_id,
                lat_nano,
                lon_nano,
            } => write!(
                f,
                "n{node_id}: node record outside the geographic domain \
                 (lat {lat_nano}, lon {lon_nano} nanodegrees)"
            ),
            LocationViolationKind::DisagreeingRefs { node_id } => write!(
                f,
                "n{node_id}: not in the file, and the ways referencing it carry \
                 different coordinates for it"
            ),
        }
    }
}

/// Result of [`check_locations`].
///
/// Every reference lands in exactly one of `refs_verified`,
/// `refs_mismatched`, `refs_not_compared`, `refs_to_conflicting_nodes`,
/// `refs_to_invalid_nodes` and `refs_without_node`, so they sum to `refs`.
#[derive(Debug, Clone, Default)]
pub struct LocationsReport {
    /// Whether the agreement check ran.
    pub agreement: bool,
    /// Partitions (passes over the way blobs) the run used. A resource
    /// decision, not a property of the file: it follows the store budget,
    /// which by default follows the host's available memory.
    pub partitions: u64,
    /// Blob decodes the run made, over every pass. A cost figure, like
    /// `partitions`.
    pub blob_decodes: u64,
    /// Node records read.
    pub nodes: u64,
    /// Ways read.
    pub ways: u64,
    /// Way node references read.
    pub refs: u64,
    /// References whose inline coordinate equals their node's record.
    pub refs_verified: u64,
    /// References whose inline coordinate differs from their node's record.
    pub refs_mismatched: u64,
    /// Distinct nodes with at least one mismatched reference.
    pub nodes_mismatched: u64,
    /// References to a node with one valid record whose way carries no
    /// coordinate arrays or whose inline coordinate is out of the domain:
    /// there is nothing to compare (the way or coordinate is reported).
    pub refs_not_compared: u64,
    /// References to a node the file has no record of. Not a violation.
    pub refs_without_node: u64,
    /// Of `refs_without_node`, those whose inline coordinate is exactly
    /// `(0, 0)`, the placeholder `add-locations-to-ways
    /// --ignore-missing-nodes` writes.
    pub refs_without_node_at_null_island: u64,
    /// Agreement mode only: distinct referenced nodes the file has no record
    /// of.
    pub nodes_absent: Option<u64>,
    /// Agreement mode: absent nodes whose references disagree.
    pub nodes_disagreeing: u64,
    /// Nodes whose valid records disagree with each other.
    pub nodes_conflicting: u64,
    /// References to a node with conflicting records (not compared), with or
    /// without an inline coordinate.
    pub refs_to_conflicting_nodes: u64,
    /// References to a node with an out-of-domain record (not compared),
    /// with or without an inline coordinate.
    pub refs_to_invalid_nodes: u64,
    /// Node records with an out-of-domain coordinate.
    pub invalid_node_records: u64,
    /// Ways with node references but no coordinate arrays.
    pub ways_without_locations: u64,
    /// References whose inline coordinate is out of the domain.
    pub refs_invalid_inline: u64,
    /// Retained violation details, at most `max_errors` unless it is zero.
    pub violations: Vec<LocationViolation>,
}

impl LocationsReport {
    /// Total violations found, which may exceed `violations.len()`.
    pub fn total_violations(&self) -> u64 {
        self.refs_mismatched
            + self.refs_invalid_inline
            + self.ways_without_locations
            + self.nodes_conflicting
            + self.invalid_node_records
            + self.nodes_disagreeing
    }

    /// True when no violation was found.
    pub fn passed(&self) -> bool {
        self.total_violations() == 0
    }

    /// Build the report as a JSON value.
    pub fn to_json_value(&self, file_name: &str) -> serde_json::Value {
        let violations: Vec<serde_json::Value> = self
            .violations
            .iter()
            .map(|v| {
                serde_json::json!({
                    "blob": v.blob_ordinal,
                    "detail": v.to_string(),
                })
            })
            .collect();
        serde_json::json!({
            "file": file_name,
            "agreement": self.agreement,
            "partitions": self.partitions,
            "blob_decodes": self.blob_decodes,
            "nodes": self.nodes,
            "ways": self.ways,
            "refs": self.refs,
            "refs_verified": self.refs_verified,
            "refs_mismatched": self.refs_mismatched,
            "nodes_mismatched": self.nodes_mismatched,
            "refs_not_compared": self.refs_not_compared,
            "refs_without_node": self.refs_without_node,
            "refs_without_node_at_null_island": self.refs_without_node_at_null_island,
            "nodes_absent": self.nodes_absent,
            "nodes_disagreeing": self.nodes_disagreeing,
            "nodes_conflicting": self.nodes_conflicting,
            "refs_to_conflicting_nodes": self.refs_to_conflicting_nodes,
            "refs_to_invalid_nodes": self.refs_to_invalid_nodes,
            "invalid_node_records": self.invalid_node_records,
            "ways_without_locations": self.ways_without_locations,
            "refs_invalid_inline": self.refs_invalid_inline,
            "passed": self.passed(),
            "total_violations": self.total_violations(),
            "violations": violations,
        })
    }

    /// Write the human-readable summary to `out`: the counts and the verdict
    /// first, then the retained violations. Counts lead so that any head of
    /// the report (a pager, or a harness quoting its first lines) says what
    /// failed and how much of it before the individual ways, of which there
    /// can be up to `max_errors`.
    ///
    /// # Errors
    ///
    /// Returns the first error `out` reports.
    pub fn write_human<W: std::io::Write>(
        &self,
        out: &mut W,
        file_name: &str,
    ) -> std::io::Result<()> {
        self.write_counts(out)?;
        if self.passed() {
            writeln!(out, "Inline locations for {file_name}: OK")?;
        } else {
            writeln!(
                out,
                "Inline locations for {file_name}: FAILED ({} violations)",
                self.total_violations()
            )?;
        }
        for v in &self.violations {
            writeln!(out, "{v}")?;
        }
        let shown = self.violations.len() as u64;
        if self.total_violations() > shown {
            writeln!(
                out,
                "... {} further violation(s) not shown (the report cap was reached)",
                self.total_violations() - shown
            )?;
        }
        Ok(())
    }

    /// The count lines of [`Self::write_human`].
    fn write_counts<W: std::io::Write>(&self, out: &mut W) -> std::io::Result<()> {
        writeln!(
            out,
            "Elements: {} nodes, {} ways, {} way node refs",
            self.nodes, self.ways, self.refs
        )?;
        write!(
            out,
            "Refs with a node record: {} verified, {} mismatched ({} nodes)",
            self.refs_verified, self.refs_mismatched, self.nodes_mismatched
        )?;
        if self.refs_not_compared > 0 {
            write!(
                out,
                ", {} without a valid inline coordinate",
                self.refs_not_compared
            )?;
        }
        writeln!(out)?;
        write!(
            out,
            "Refs without a node record (not verifiable): {}",
            self.refs_without_node
        )?;
        if self.refs_without_node_at_null_island > 0 {
            write!(
                out,
                ", {} of them at 0,0",
                self.refs_without_node_at_null_island
            )?;
        }
        writeln!(out)?;
        if let Some(absent) = self.nodes_absent {
            writeln!(
                out,
                "Nodes not in the file: {absent}, {} with disagreeing inline coordinates",
                self.nodes_disagreeing
            )?;
        }
        if self.nodes_conflicting > 0 || self.invalid_node_records > 0 {
            writeln!(
                out,
                "Node records: {} nodes with conflicting records, {} records outside \
                 the domain ({} and {} refs not compared)",
                self.nodes_conflicting,
                self.invalid_node_records,
                self.refs_to_conflicting_nodes,
                self.refs_to_invalid_nodes
            )?;
        }
        if self.ways_without_locations > 0 || self.refs_invalid_inline > 0 {
            writeln!(
                out,
                "Inline coordinates: {} ways without any, {} refs outside the domain",
                self.ways_without_locations, self.refs_invalid_inline
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Retained details
// ---------------------------------------------------------------------------

/// The `cap` smallest violations by key; `cap == 0` keeps all of them. The
/// set of violations found does not depend on worker timing, so neither does
/// this selection.
struct Details {
    cap: usize,
    items: Vec<LocationViolation>,
}

impl Details {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            items: Vec::new(),
        }
    }

    fn push(
        &mut self,
        blob_ordinal: Option<u64>,
        key: (u8, u64, u64, u64),
        kind: LocationViolationKind,
    ) {
        self.absorb_one(LocationViolation {
            blob_ordinal,
            kind,
            key,
        });
    }

    fn absorb_one(&mut self, v: LocationViolation) {
        self.items.push(v);
        if self.cap != 0 && self.items.len() >= self.cap.saturating_mul(2).max(64) {
            self.trim();
        }
    }

    fn absorb(&mut self, other: Vec<LocationViolation>) {
        for v in other {
            self.absorb_one(v);
        }
    }

    fn trim(&mut self) {
        self.items.sort_unstable_by_key(|v| v.key);
        if self.cap != 0 {
            self.items.truncate(self.cap);
        }
    }

    fn finish(mut self) -> Vec<LocationViolation> {
        self.trim();
        self.items
    }
}

/// Key categories, in report order.
const KEY_MISMATCH: u8 = 0;
const KEY_INVALID_INLINE: u8 = 1;
const KEY_WAY_WITHOUT: u8 = 2;
const KEY_CONFLICTING: u8 = 3;
const KEY_INVALID_NODE: u8 = 4;
const KEY_DISAGREEING: u8 = 5;

/// A non-negative ID as a key component.
#[allow(clippy::cast_sign_loss)]
fn id_key(id: i64) -> u64 {
    id.max(0) as u64
}

// ---------------------------------------------------------------------------
// The key set
// ---------------------------------------------------------------------------

/// The key set under construction, sharded by `IdSet` chunk so workers
/// insert straight into it: no ID vector per blob is queued for a merge
/// thread, and the shards hold disjoint chunks, so joining them moves
/// chunks rather than copying them.
struct KeyShards {
    shards: Vec<Mutex<IdSet>>,
}

impl KeyShards {
    fn new() -> Self {
        Self {
            shards: (0..CHECK_LOCATIONS_KEY_SHARDS)
                .map(|_| Mutex::new(IdSet::new()))
                .collect(),
        }
    }

    /// Insert and clear a worker's batch. Negative IDs are not keyed.
    fn flush(&self, batch: &mut Vec<i64>) {
        batch.sort_unstable();
        let mut rest = batch.as_slice();
        while let Some(&first) = rest.first() {
            let chunk = IdSet::chunk_of(first);
            let run = rest
                .iter()
                .position(|&id| IdSet::chunk_of(id) != chunk)
                .unwrap_or(rest.len());
            if let Some(chunk) = chunk {
                let shard = usize::try_from(chunk % self.shards.len() as u64).unwrap_or(0);
                let mut set = self.shards[shard]
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for &id in &rest[..run] {
                    set.set(id);
                }
            }
            rest = &rest[run..];
        }
        batch.clear();
    }

    /// Add one ID to a worker's batch, inserting when the batch is full.
    fn add(&self, batch: &mut Vec<i64>, id: i64) {
        batch.push(id);
        if batch.len() >= CHECK_LOCATIONS_KEY_BATCH {
            self.flush(batch);
        }
    }

    fn into_set(self) -> IdSet {
        let mut out = IdSet::new();
        for shard in self.shards {
            out.merge(
                shard
                    .into_inner()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The walk and the schedules
// ---------------------------------------------------------------------------

/// What routing knows about one blob.
#[derive(Clone, Copy, Debug)]
struct BlobFacts {
    blob: u64,
    /// The indexdata kind, if the blob carries readable indexdata.
    claim: Option<ElementKind>,
    /// Kinds the up-front pass saw in the blob; empty for a blob it did not
    /// read (one claiming ways).
    kinds: Kinds,
    /// Node-ID range the up-front pass saw; inverted when it saw no node.
    min_node: i64,
    max_node: i64,
}

/// One schedule and its blobs' facts, by schedule position.
#[derive(Default)]
struct Schedule {
    blobs: Vec<ScheduledBlob>,
    facts: Vec<BlobFacts>,
}

impl Schedule {
    fn push(&mut self, blob: ScheduledBlob, facts: BlobFacts) {
        self.blobs.push(blob);
        self.facts.push(facts);
    }

    fn len(&self) -> u64 {
        self.blobs.len() as u64
    }
}

struct Walk {
    file: InputFile,
    /// Every blob not claiming ways: the up-front pass.
    prepass: Schedule,
    /// Blobs claiming ways, which only the way passes read.
    claimed_ways: Schedule,
}

fn walk(input: &PinnedInput, host: &crate::host::Entered<'_>) -> Result<Walk> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let first = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;
    if first.blob_type != crate::blob::BlobKind::OsmHeader {
        return Err(crate::error::new_missing_header(input.path()));
    }
    let mut out = Walk {
        file: walker.input(),
        prepass: Schedule::default(),
        claimed_ways: Schedule::default(),
    };
    while let Some(meta) = walker.next_header()? {
        if meta.blob_type != crate::blob::BlobKind::OsmData {
            continue;
        }
        // Indexdata the reader cannot use (a reversed range among others)
        // reads as `None` here, as on every walk: in a file whose other data
        // blobs are indexed the walk refuses it as partially indexed, and in
        // a wholly unindexed file it is an ordinary unindexed blob.
        let facts = BlobFacts {
            blob: meta.blob_index,
            claim: meta.index.as_ref().map(|i| i.kind),
            kinds: Kinds::default(),
            min_node: i64::MAX,
            max_node: i64::MIN,
        };
        // Unchecked: this check routes on what its raw scanners decode and
        // makes its own claim test (`scan::scan_ways`).
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Unchecked)?;
        if facts.claim == Some(ElementKind::Way) {
            out.claimed_ways.push(entry, facts);
        } else {
            out.prepass.push(entry, facts);
        }
    }
    Ok(out)
}

/// The node and way schedules, from what the up-front pass saw.
fn route(walk: &Walk) -> (Schedule, Schedule) {
    let mut nodes = Schedule::default();
    let mut ways = Schedule::default();
    for (blob, facts) in walk.prepass.blobs.iter().zip(&walk.prepass.facts) {
        if facts.kinds.has(ElementKind::Node) && facts.min_node <= facts.max_node {
            nodes.push(*blob, *facts);
        }
        if facts.kinds.has(ElementKind::Way) {
            ways.push(*blob, *facts);
        }
    }
    for (blob, facts) in walk.claimed_ways.blobs.iter().zip(&walk.claimed_ways.facts) {
        ways.push(*blob, *facts);
    }
    (nodes, ways)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Verify a LocationsOnWays file's inline way-node coordinates against its
/// node records. See the module documentation for what is a violation and
/// what is only counted.
///
/// # Errors
///
/// Refuses, rather than reports, a file it cannot check: one whose header
/// does not declare `LocationsOnWays` (`MissingFeature`) or declares
/// `HistoricalInformation` (several versions of a node legitimately carry
/// different coordinates); a partially indexed file; a way referencing a
/// negative node ID (ADR-0002); and any data blob the reader refuses to
/// build a block from (every blob is validated once as the reader validates
/// it).
///
/// A blob whose indexdata claims ways while it holds elements of another
/// kind is not a refusal but a stopped check
/// ([`CheckOutcome::Stopped`](super::CheckOutcome)): the check reads such a
/// blob only in its way passes, so the claim is false and the file fails.
///
/// # Input
///
/// The input is read in several passes and is pinned for the run (see
/// [`InputSource::pin`]), so it must be a regular file.
pub fn check_locations<'a>(
    input: impl Into<InputSource<'a>>,
    opts: &CheckLocationsOptions,
    host: &crate::host::Host,
) -> Result<super::CheckOutcome<LocationsReport>> {
    let host = &host.enter()?;
    let input = input.into();
    host.invocation.decided(|| {
        super::CheckOutcome::stop_on_contradiction(check_locations_inner(input, opts, host))
    })
}

/// What one blob contributes to the up-front pass.
struct PrepassBlob {
    nodes: u64,
    invalid: u64,
    kinds: Kinds,
    min_node: i64,
    max_node: i64,
    details: Vec<LocationViolation>,
}

/// Per-worker state of the up-front and key passes.
#[derive(Default)]
struct KeyWorker {
    groups: Groups,
    batch: Vec<i64>,
    validate: ValidateScratch,
}

/// Totals one way blob contributes to one partition's way phase.
#[derive(Default)]
struct WayBlob {
    ways: u64,
    refs: u64,
    verified: u64,
    mismatched: u64,
    not_compared: u64,
    without_node: u64,
    null_island: u64,
    to_conflicting: u64,
    to_invalid: u64,
    ways_without_locations: u64,
    invalid_inline: u64,
    details: Vec<LocationViolation>,
}

fn check_locations_inner(
    input: InputSource<'_>,
    opts: &CheckLocationsOptions,
    host: &crate::host::Entered<'_>,
) -> Result<LocationsReport> {
    let pin = input.pin(host)?;
    {
        let mut front = crate::commands::input_front::InputFront::open(&pin, false, host)?;
        let header = front.header()?;
        if !header.has_locations_on_ways() {
            return Err(crate::error::new_missing_feature(
                crate::HeaderBlock::LOCATIONS_ON_WAYS,
            ));
        }
        if header.has_historical_information() {
            return Err(Error::invalid_argument(
                "the location check does not check history files: several \
                 versions of one node legitimately carry different coordinates",
            ));
        }
    }
    // Read before any blob: once the passes have streamed the file, a host
    // that charges page cache to the process (a cgroup v2 limit) reports far
    // less available than the run can use.
    let available = host.resources.memory_available();
    host.process.cross_thread_alloc_pool();
    let mut walk = walk(&pin, host)?;
    let cap = opts.check.max_errors;
    let mut details = Details::new(cap);
    let mut report = LocationsReport {
        agreement: opts.agreement,
        ..LocationsReport::default()
    };

    let shards = KeyShards::new();
    host.invocation.marker("CHECKLOC_NODES_START");
    prepass(&mut walk, &shards, cap, &mut report, &mut details, host)?;
    host.invocation.marker("CHECKLOC_NODES_END");
    let (nodes, ways) = route(&walk);

    if opts.agreement {
        host.invocation.marker("CHECKLOC_REFS_START");
        key_refs(&walk.file, &ways, &shards, host)?;
        report.blob_decodes += ways.len();
        host.invocation.marker("CHECKLOC_REFS_END");
    }

    let keys = shards.into_set();
    let key_bytes = key_set_bytes(&keys);
    let keys = keys.into_ranked();
    let budget = store_budget(opts, available, key_bytes);
    let plan = Plan::new(&keys, slots_for_budget(budget));
    report.partitions = plan.partitions() as u64;
    if opts.agreement {
        report.nodes_absent = Some(0);
    }
    host.invocation.counter("checkloc_keys", keys.total_count());
    host.invocation
        .counter("checkloc_partitions", report.partitions);
    if plan.partitions() > CHECK_LOCATIONS_PARTITION_WARN {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "check.locations_partitions",
            "the location check will read every way blob {} times: {} node IDs \
             need a slot and the coordinate store budget is {} MiB",
            plan.partitions(),
            keys.total_count(),
            budget >> 20
        );
    }

    let ctx = RunCtx {
        file: &walk.file,
        nodes: &nodes,
        ways: &ways,
        keys: &keys,
        plan: &plan,
        agreement: opts.agreement,
        cap,
    };
    for k in 0..plan.partitions() {
        host.invocation.check()?;
        host.invocation.marker("CHECKLOC_PARTITION_START");
        ctx.run_partition(k, &mut report, &mut details, host)?;
        host.invocation.marker("CHECKLOC_PARTITION_END");
    }

    report.violations = details.finish();
    host.invocation
        .counter("checkloc_blob_decodes", report.blob_decodes);
    host.invocation
        .counter("checkloc_refs_mismatched", report.refs_mismatched);
    host.invocation
        .counter("checkloc_refs_without_node", report.refs_without_node);
    Ok(report)
}

/// The up-front pass over every blob not claiming ways: validate it, count
/// its node records, key their IDs and record what it holds.
fn prepass(
    walk: &mut Walk,
    shards: &KeyShards,
    cap: usize,
    report: &mut LocationsReport,
    details: &mut Details,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let facts = &walk.prepass.facts;
    let mut seen: Vec<(Kinds, i64, i64)> =
        vec![(Kinds::default(), i64::MAX, i64::MIN); facts.len()];
    parallel_scan_blobs_raw(
        &walk.file,
        &walk.prepass.blobs,
        None,
        KeyWorker::default,
        |pos, block, worker| -> Result<PrepassBlob> {
            validate_block(block, &mut worker.validate)?;
            let blob = facts[pos].blob;
            let mut local = Details::new(cap);
            let mut out = PrepassBlob {
                nodes: 0,
                invalid: 0,
                kinds: Kinds::default(),
                min_node: i64::MAX,
                max_node: i64::MIN,
                details: Vec::new(),
            };
            let KeyWorker { groups, batch, .. } = worker;
            let kinds = scan_nodes(block, groups, |ordinal, id, coord| {
                out.nodes += 1;
                out.min_node = out.min_node.min(id);
                out.max_node = out.max_node.max(id);
                shards.add(batch, id);
                if let Coord::Invalid { lat_nano, lon_nano } = coord {
                    out.invalid += 1;
                    local.push(
                        Some(blob),
                        (KEY_INVALID_NODE, blob, ordinal, 0),
                        LocationViolationKind::InvalidNodeRecord {
                            node_id: id,
                            lat_nano,
                            lon_nano,
                        },
                    );
                }
                Ok(())
            })?;
            out.kinds = kinds;
            shards.flush(batch);
            out.details = local.finish();
            Ok(out)
        },
        |pos, blob| {
            report.nodes += blob.nodes;
            report.invalid_node_records += blob.invalid;
            seen[pos] = (blob.kinds, blob.min_node, blob.max_node);
            details.absorb(blob.details);
        },
        host,
    )?;
    for (facts, (kinds, min_node, max_node)) in walk.prepass.facts.iter_mut().zip(seen) {
        facts.kinds = kinds;
        facts.min_node = min_node;
        facts.max_node = max_node;
    }
    report.blob_decodes += walk.prepass.len();
    Ok(())
}

/// Agreement mode: key every way reference too.
fn key_refs(
    file: &InputFile,
    ways: &Schedule,
    shards: &KeyShards,
    host: &crate::host::Entered<'_>,
) -> Result<()> {
    let facts = &ways.facts;
    parallel_scan_blobs_raw(
        file,
        &ways.blobs,
        None,
        KeyWorker::default,
        |pos, block, worker| -> Result<()> {
            let KeyWorker { groups, batch, .. } = worker;
            scan_ways(block, facts[pos].claim, groups, |event| {
                if let WayEvent::Ref { node_id, .. } = *event {
                    shards.add(batch, node_id);
                }
                Ok(())
            })?;
            shards.flush(batch);
            Ok(())
        },
        |_pos, ()| {},
        host,
    )
}

/// An upper bound on the key set's memory and its rank index.
fn key_set_bytes(keys: &IdSet) -> u64 {
    keys.ranked_bytes_upper_bound()
}

/// The store budget in bytes: the caller's, or a share of the memory the
/// host had available before the run read any blob, less the headroom and
/// the key set.
fn store_budget(opts: &CheckLocationsOptions, available: Option<u64>, key_bytes: u64) -> u64 {
    if let Some(bytes) = opts.store_budget_bytes {
        return bytes;
    }
    available.map_or(CHECK_LOCATIONS_FALLBACK_BUDGET_BYTES, |available| {
        (available
            .saturating_sub(CHECK_LOCATIONS_HEADROOM_BYTES)
            .saturating_sub(key_bytes)
            / 100
            * CHECK_LOCATIONS_STORE_BUDGET_PCT)
            .max(CHECK_LOCATIONS_MIN_BUDGET_BYTES)
    })
}

#[cold]
fn negative_ref_error(way_id: i64, node_id: i64) -> Error {
    Error::invalid_data(format!(
        "way {way_id} references negative node ID {node_id}; the location \
         check does not check negative IDs, which no production PBF carries"
    ))
}

/// Slot of `rank` within a partition starting at `first_rank`. Both come
/// from the plan the store was sized by, so the index is in bounds.
#[allow(clippy::cast_possible_truncation)]
fn slot_index(rank: u64, first_rank: u64) -> usize {
    (rank - first_rank) as usize
}

// ---------------------------------------------------------------------------
// Partitions
// ---------------------------------------------------------------------------

/// What every partition shares.
struct RunCtx<'a> {
    file: &'a InputFile,
    nodes: &'a Schedule,
    ways: &'a Schedule,
    keys: &'a RankedIdSet,
    plan: &'a Plan,
    agreement: bool,
    cap: usize,
}

impl RunCtx<'_> {
    /// One partition: join its node records, then check every way reference
    /// it owns, then count the final slot states.
    fn run_partition(
        &self,
        k: usize,
        report: &mut LocationsReport,
        details: &mut Details,
        host: &crate::host::Entered<'_>,
    ) -> Result<()> {
        let range = self.plan.ids(k);
        let (first_rank, len) = self.plan.ranks(k);
        let store = Store::new(len)?;
        if len > 0 {
            self.node_phase(range, first_rank, &store, report, host)?;
        }
        self.way_phase(k, range, first_rank, &store, report, details, host)?;
        self.finish_partition(range, &store, len, report, details);
        Ok(())
    }

    /// The blobs holding nodes in the partition's interval, as the up-front
    /// pass saw them; each record in the interval joins its slot.
    fn node_phase(
        &self,
        range: (i64, Option<i64>),
        first_rank: u64,
        store: &Store,
        report: &mut LocationsReport,
        host: &crate::host::Entered<'_>,
    ) -> Result<()> {
        let mut blobs = Vec::new();
        for (blob, facts) in self.nodes.blobs.iter().zip(&self.nodes.facts) {
            if range_meets(facts.min_node, facts.max_node, range) {
                blobs.push(*blob);
            }
        }
        let keys = self.keys;
        parallel_scan_blobs_raw(
            self.file,
            &blobs,
            None,
            Groups::new,
            |_pos, block, groups| -> Result<()> {
                scan_nodes(block, groups, |_ordinal, id, coord| {
                    if in_partition(id, range)
                        && let Some(rank) = keys.rank_if_set(id)
                    {
                        store.join_node(slot_index(rank, first_rank), coord);
                    }
                    Ok(())
                })
                .map(drop)
            },
            |_pos, ()| {},
            host,
        )?;
        report.blob_decodes += blobs.len() as u64;
        Ok(())
    }

    /// Every way blob. The node phase's pool join is the barrier that makes
    /// every node join visible here.
    #[allow(clippy::too_many_arguments)]
    fn way_phase(
        &self,
        k: usize,
        range: (i64, Option<i64>),
        first_rank: u64,
        store: &Store,
        report: &mut LocationsReport,
        details: &mut Details,
        host: &crate::host::Entered<'_>,
    ) -> Result<()> {
        let facts = &self.ways.facts;
        let cap = self.cap;
        parallel_scan_blobs_raw(
            self.file,
            &self.ways.blobs,
            None,
            KeyWorker::default,
            |pos, block, worker| -> Result<WayBlob> {
                let ctx = WayCtx {
                    keys: self.keys,
                    store,
                    range,
                    first_rank,
                    designated: k == 0,
                    agreement: self.agreement,
                    facts: facts[pos],
                };
                // A blob claiming ways was not read by the up-front pass, so
                // the designated partition validates it.
                if ctx.designated && ctx.facts.claim == Some(ElementKind::Way) {
                    validate_block(block, &mut worker.validate)?;
                }
                let mut out = WayBlob::default();
                let mut local = Details::new(cap);
                let mut way = (0_u64, 0_i64);
                scan_ways(block, ctx.facts.claim, &mut worker.groups, |event| {
                    match *event {
                        WayEvent::Way {
                            ordinal,
                            id,
                            has_refs,
                            has_coords,
                        } => {
                            way = (ordinal, id);
                            if ctx.designated {
                                ctx.way_start(way, has_refs, has_coords, &mut out, &mut local);
                            }
                        }
                        WayEvent::Ref {
                            position, node_id, ..
                        } => {
                            if ctx.designated {
                                ctx.structural_ref(
                                    way, position, node_id, event, &mut out, &mut local,
                                )?;
                            }
                            ctx.compare(way, position, node_id, event, &mut out, &mut local);
                        }
                    }
                    Ok(())
                })?;
                out.details = local.finish();
                Ok(out)
            },
            |_pos, blob| {
                report.ways += blob.ways;
                report.refs += blob.refs;
                report.refs_verified += blob.verified;
                report.refs_mismatched += blob.mismatched;
                report.refs_not_compared += blob.not_compared;
                report.refs_without_node += blob.without_node;
                report.refs_without_node_at_null_island += blob.null_island;
                report.refs_to_conflicting_nodes += blob.to_conflicting;
                report.refs_to_invalid_nodes += blob.to_invalid;
                report.ways_without_locations += blob.ways_without_locations;
                report.refs_invalid_inline += blob.invalid_inline;
                details.absorb(blob.details);
            },
            host,
        )?;
        report.blob_decodes += self.ways.len();
        Ok(())
    }

    /// Count the final slot states and retain the node-ID details.
    fn finish_partition(
        &self,
        range: (i64, Option<i64>),
        store: &Store,
        len: u64,
        report: &mut LocationsReport,
        details: &mut Details,
    ) {
        let cap = self.cap;
        let fin = store.finish();
        let conflicting_before = report.nodes_conflicting;
        let disagreeing_before = report.nodes_disagreeing;
        report.nodes_conflicting += fin.conflicting;
        report.nodes_mismatched += fin.mismatched;
        report.nodes_disagreeing += fin.disagreeing;
        if let Some(absent) = report.nodes_absent.as_mut() {
            *absent += len - fin.present;
        }
        // Node-ID details, smallest IDs first. Partitions run in ID order, so
        // the ones earlier partitions already retained are the smallest; this
        // one needs only what the cap leaves of each category.
        let still_wanted = |found_before: u64, found_here: u64| -> u64 {
            if cap == 0 {
                found_here
            } else {
                found_here.min((cap as u64).saturating_sub(found_before))
            }
        };
        let mut want_conflicting = still_wanted(conflicting_before, fin.conflicting);
        let mut want_disagreeing = still_wanted(disagreeing_before, fin.disagreeing);
        if want_conflicting + want_disagreeing == 0 {
            return;
        }
        let (lo, _) = range;
        for (i, id) in self.keys.iter_from(lo).take(store.len()).enumerate() {
            match store.reportable(i) {
                Some(SlotState::Conflict) if want_conflicting > 0 => {
                    want_conflicting -= 1;
                    details.push(
                        None,
                        (KEY_CONFLICTING, id_key(id), 0, 0),
                        LocationViolationKind::ConflictingNode { node_id: id },
                    );
                }
                Some(SlotState::Disagree) if want_disagreeing > 0 => {
                    want_disagreeing -= 1;
                    details.push(
                        None,
                        (KEY_DISAGREEING, id_key(id), 0, 0),
                        LocationViolationKind::DisagreeingRefs { node_id: id },
                    );
                }
                _ => {}
            }
            if want_conflicting + want_disagreeing == 0 {
                break;
            }
        }
    }
}

/// What one way-phase worker needs to check a blob against one partition.
struct WayCtx<'a> {
    keys: &'a RankedIdSet,
    store: &'a Store,
    range: (i64, Option<i64>),
    first_rank: u64,
    /// The partition that also makes the partition-independent checks.
    designated: bool,
    agreement: bool,
    facts: BlobFacts,
}

impl WayCtx<'_> {
    /// A way begins: count it, and report it if it has references but no
    /// coordinate arrays. Designated partition only.
    fn way_start(
        &self,
        (ordinal, id): (u64, i64),
        has_refs: bool,
        has_coords: bool,
        out: &mut WayBlob,
        local: &mut Details,
    ) {
        out.ways += 1;
        if has_refs && !has_coords {
            out.ways_without_locations += 1;
            local.push(
                Some(self.facts.blob),
                (KEY_WAY_WITHOUT, self.facts.blob, ordinal, 0),
                LocationViolationKind::WayWithoutLocations { way_id: id },
            );
        }
    }

    /// The facts about one reference that do not depend on the partition:
    /// the count, a negative ID, an out-of-domain inline coordinate.
    /// Designated partition only.
    fn structural_ref(
        &self,
        (ordinal, way_id): (u64, i64),
        position: u64,
        node_id: i64,
        event: &WayEvent,
        out: &mut WayBlob,
        local: &mut Details,
    ) -> Result<()> {
        out.refs += 1;
        if node_id < 0 {
            return Err(negative_ref_error(way_id, node_id));
        }
        if let Some(Coord::Invalid { lat_nano, lon_nano }) = event.coord() {
            out.invalid_inline += 1;
            local.push(
                Some(self.facts.blob),
                (KEY_INVALID_INLINE, self.facts.blob, ordinal, position),
                LocationViolationKind::InvalidInline {
                    way_id,
                    node_id,
                    position,
                    lat_nano,
                    lon_nano,
                },
            );
        }
        Ok(())
    }

    /// Check one reference, if this partition owns it.
    fn compare(
        &self,
        (ordinal, way_id): (u64, i64),
        position: u64,
        node_id: i64,
        event: &WayEvent,
        out: &mut WayBlob,
        local: &mut Details,
    ) {
        if !in_partition(node_id, self.range) {
            return;
        }
        // Scaled only for an owned reference.
        let coord = event.coord();
        let slot = self
            .keys
            .rank_if_set(node_id)
            .map(|rank| slot_index(rank, self.first_rank));
        let Some(i) = slot.filter(|&i| self.store.has_authority(i)) else {
            out.without_node += 1;
            if coord == Some(Coord::Valid { lat: 0, lon: 0 }) {
                out.null_island += 1;
            }
            if self.agreement
                && let (Some(i), Some(Coord::Valid { lat, lon })) = (slot, coord)
            {
                self.store.agree(i, lat, lon);
            }
            return;
        };
        match self.store.state(i) {
            SlotState::Coord {
                lat: node_lat,
                lon: node_lon,
            } => {
                // An out-of-domain inline coordinate, or a way without any,
                // was reported by the structural checks; nothing to compare.
                let Some(Coord::Valid { lat, lon }) = coord else {
                    out.not_compared += 1;
                    return;
                };
                if (lat, lon) == (node_lat, node_lon) {
                    out.verified += 1;
                } else {
                    out.mismatched += 1;
                    self.store.mark_mismatch(i);
                    local.push(
                        Some(self.facts.blob),
                        (KEY_MISMATCH, self.facts.blob, ordinal, position),
                        LocationViolationKind::Mismatch {
                            way_id,
                            node_id,
                            position,
                            inline: (lat, lon),
                            node: (node_lat, node_lon),
                        },
                    );
                }
            }
            SlotState::Conflict => out.to_conflicting += 1,
            SlotState::Unusable => out.to_invalid += 1,
            // A slot with a node record holds one of the three states above
            // once the node phase has joined; counted so the identity on
            // `LocationsReport` holds even if it did not.
            SlotState::Empty | SlotState::Disagree => out.not_compared += 1,
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::{CheckLocationsOptions, store_budget};

    const GIB: u64 = 1 << 30;

    fn opts(pinned: Option<u64>) -> CheckLocationsOptions {
        CheckLocationsOptions {
            check: super::super::CheckOptions {
                max_errors: 0,
                direct_io: false,
            },
            agreement: false,
            store_budget_bytes: pinned,
        }
    }

    /// The budget is a share of what was available less the headroom and
    /// the key set; an unknown host gets the fallback, a spent one the
    /// floor, and a pinned budget is taken as given.
    #[test]
    fn the_budget_follows_memory_measured_before_the_run_less_the_key_set() {
        assert_eq!(
            store_budget(&opts(None), Some(12 * GIB), GIB),
            9 * GIB / 100 * 60
        );
        assert_eq!(
            store_budget(&opts(None), None, GIB),
            crate::tunables::CHECK_LOCATIONS_FALLBACK_BUDGET_BYTES
        );
        assert_eq!(
            store_budget(&opts(None), Some(GIB), 4 * GIB),
            crate::tunables::CHECK_LOCATIONS_MIN_BUDGET_BYTES
        );
        assert_eq!(store_budget(&opts(Some(7)), Some(12 * GIB), GIB), 7);
    }
}
