//! An independent PBF decoder and the element model successful-output
//! oracles compare.
//!
//! Nothing here goes through pbfhogg's reader: frames are walked by
//! [`super::adversarial::locate_blobs_strict`], the Blob is decompressed with
//! `flate2` or `zstd`, and every message is parsed by hand. So a decoder
//! defect cannot make two outputs agree, and an output field the production
//! accessors cannot show (an absent plain `visible`, say) is still compared.
//!
//! The decode is lossless where the format can tell values apart, and refuses
//! (panics) where it cannot read the file: a malformed field, a known field
//! of the wrong wire type, parallel arrays of different lengths, a string
//! index past the table, a payload whose `raw_size` disagrees. A malformed
//! value never becomes an empty or default one. Unknown fields are skipped by
//! wire type, and repeated scalars are read packed or unpacked, split over any
//! number of occurrences, as protobuf specifies; a singular message that
//! occurs more than once is merged by concatenation, which is protobuf's
//! merge.
//!
//! What it keeps:
//!
//! - Coordinates in exact nanodegrees, for nodes and for a way's embedded
//!   locations (`LocationsOnWays`), offsets and granularity applied.
//! - Tags as stored: in order, duplicate keys included.
//! - Member kinds with the raw value of an unknown one.
//! - Metadata as the claims CORRECTNESS.md "Version 0 is not a version claim
//!   either" defines, field by field, timestamps in milliseconds. A plain
//!   `Info` claims each field it carries (a version only at 1 or more, a
//!   changeset only when not the Osmosis -1). A dense slot with a version of
//!   1 or more claims each column its group carries, a changeset of -1 as 0,
//!   and `visible` always (`true` without its column); a slot below version 1
//!   claims only values padding never holds (a non-zero timestamp, a positive
//!   changeset or uid, a non-empty user, `visible` false). An element that
//!   claims nothing has `meta: None`.
//!
//! What it does not hold: the file header's content (only that a header
//! precedes the data), BlobHeader extensions (indexdata, tagdata,
//! waymembers) and a way's shared-node pins. Assert those separately where an
//! operation promises them.
//!
//! The re-encoders may rewrite some claims (DEVIATIONS.md "Re-encoding: absent
//! metadata fields stay absent", and sub-second timestamps truncated to whole
//! seconds); this model reports such a rewrite as a difference, and a test
//! of such an operation states the rewrite as its expected output.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::fmt::Display;
use std::io::Read;
use std::path::Path;

use flate2::read::ZlibDecoder;

use super::adversarial::{locate_blobs_strict, read_varint};

/// Element metadata as claimed, field by field; `None` is "not claimed".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct NormalizedMeta {
    pub version: Option<i32>,
    pub milli_timestamp: Option<i64>,
    pub changeset: Option<i64>,
    pub uid: Option<i32>,
    pub user: Option<String>,
    pub visible: Option<bool>,
}

impl NormalizedMeta {
    fn into_claimed(self) -> Option<Self> {
        (self != Self::default()).then_some(self)
    }
}

/// A node: id, nanodegree coordinates, tags as stored, claimed metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizedNode {
    pub id: i64,
    pub nano_lat: i64,
    pub nano_lon: i64,
    pub tags: Vec<(String, String)>,
    pub meta: Option<NormalizedMeta>,
}

impl NormalizedNode {
    /// Latitude in decimicrodegrees, truncated toward zero as the product's
    /// accessor does.
    pub fn decimicro_lat(&self) -> i32 {
        (self.nano_lat / 100) as i32
    }

    /// Longitude in decimicrodegrees; see [`decimicro_lat`](Self::decimicro_lat).
    pub fn decimicro_lon(&self) -> i32 {
        (self.nano_lon / 100) as i32
    }

    /// The value of the first tag with key `key`.
    pub fn tag(&self, key: &str) -> Option<&str> {
        first_tag(&self.tags, key)
    }
}

/// A way: id, refs, embedded nanodegree locations when it carries them, tags
/// as stored, claimed metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizedWay {
    pub id: i64,
    pub refs: Vec<i64>,
    /// `None` when the way carries no location arrays (an empty pair reads
    /// the same: the public iterator cannot tell them apart either).
    pub locations: Option<Vec<(i64, i64)>>,
    pub tags: Vec<(String, String)>,
    pub meta: Option<NormalizedMeta>,
}

impl NormalizedWay {
    /// The value of the first tag with key `key`.
    pub fn tag(&self, key: &str) -> Option<&str> {
        first_tag(&self.tags, key)
    }
}

/// A relation member's kind, an unknown wire value kept as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberKind {
    Node,
    Way,
    Relation,
    Unknown(i32),
}

impl MemberKind {
    /// The OSM name of a known kind. Panics on an unknown one rather than
    /// naming it something a known kind could also be; compare the enum
    /// where unknown kinds can occur.
    #[track_caller]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
            Self::Unknown(raw) => panic!("member kind {raw} has no OSM name"),
        }
    }
}

impl PartialEq<&str> for MemberKind {
    fn eq(&self, other: &&str) -> bool {
        !matches!(self, Self::Unknown(_)) && self.as_str() == *other
    }
}

/// A relation member: kind, ref id, role.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizedMember {
    pub member_type: MemberKind,
    pub ref_id: i64,
    pub role: String,
}

/// A relation: id, members in order, tags as stored, claimed metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NormalizedRelation {
    pub id: i64,
    pub members: Vec<NormalizedMember>,
    pub tags: Vec<(String, String)>,
    pub meta: Option<NormalizedMeta>,
}

impl NormalizedRelation {
    /// The value of the first tag with key `key`.
    pub fn tag(&self, key: &str) -> Option<&str> {
        first_tag(&self.tags, key)
    }
}

fn first_tag<'t>(tags: &'t [(String, String)], key: &str) -> Option<&'t str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Every element of a file, each kind in file order.
#[derive(Debug, Default)]
pub struct DecodedPbf {
    /// An `OSMHeader` frame precedes every `OSMData` frame (the decode
    /// refuses data before a header); false when the file has no header.
    pub header: bool,
    pub nodes: Vec<NormalizedNode>,
    pub ways: Vec<NormalizedWay>,
    pub relations: Vec<NormalizedRelation>,
}

/// Decode the PBF file at `path`. Panics on anything it cannot read.
pub fn decode_path(path: &Path) -> DecodedPbf {
    let bytes = std::fs::read(path).expect("read PBF for independent decode");
    decode_bytes(&bytes)
}

/// Decode a PBF byte stream. Panics on anything it cannot read.
pub fn decode_bytes(pbf: &[u8]) -> DecodedPbf {
    let frames = locate_blobs_strict(pbf).unwrap_or_else(|e| bad("framing", e));
    let mut out = DecodedPbf::default();
    for (location, kind) in frames.frames {
        let blob = &pbf[location.blob_start..location.blob_end];
        match kind.as_slice() {
            b"OSMHeader" => out.header = true,
            b"OSMData" => {
                if !out.header {
                    bad("framing", "OSMData frame before any OSMHeader");
                }
                decode_block(&blob_payload(blob), &mut out);
            }
            _ => {}
        }
    }
    out
}

#[cold]
#[track_caller]
fn bad(what: &str, msg: impl Display) -> ! {
    panic!("independent PBF decode, {what}: {msg}")
}

// ---------------------------------------------------------------------------
// Protobuf primitives
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
    Fixed,
}

struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
    what: &'static str,
}

fn fields<'a>(buf: &'a [u8], what: &'static str) -> Fields<'a> {
    Fields { buf, pos: 0, what }
}

impl<'a> Fields<'a> {
    fn varint(&mut self) -> u64 {
        let (value, n) =
            read_varint(&self.buf[self.pos..]).unwrap_or_else(|| bad(self.what, "bad varint"));
        self.pos += n;
        value
    }

    fn skip(&mut self, n: usize) {
        self.pos = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .unwrap_or_else(|| bad(self.what, "fixed field past the end"));
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Wire<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.varint();
        let field = tag >> 3;
        if field == 0 {
            bad(self.what, "field number 0");
        }
        let wire = match tag & 7 {
            0 => Wire::Varint(self.varint()),
            1 => {
                self.skip(8);
                Wire::Fixed
            }
            2 => {
                let len = usize::try_from(self.varint())
                    .unwrap_or_else(|_| bad(self.what, "length overflow"));
                let start = self.pos;
                self.skip(len);
                Wire::Len(&self.buf[start..self.pos])
            }
            5 => {
                self.skip(4);
                Wire::Fixed
            }
            other => bad(self.what, format!("wire type {other} on field {field}")),
        };
        Some((field, wire))
    }
}

#[track_caller]
fn scalar(wire: Wire<'_>, what: &str) -> u64 {
    match wire {
        Wire::Varint(v) => v,
        _ => bad(what, "expected a varint"),
    }
}

#[track_caller]
fn bytes<'a>(wire: Wire<'a>, what: &str) -> &'a [u8] {
    match wire {
        Wire::Len(b) => b,
        _ => bad(what, "expected a length-delimited field"),
    }
}

/// Append one occurrence of a repeated varint field, packed or unpacked.
#[track_caller]
fn repeated(out: &mut Vec<u64>, wire: Wire<'_>, what: &str) {
    match wire {
        Wire::Varint(v) => out.push(v),
        Wire::Len(mut packed) => {
            while !packed.is_empty() {
                let (v, n) = read_varint(packed).unwrap_or_else(|| bad(what, "bad packed varint"));
                out.push(v);
                packed = &packed[n..];
            }
        }
        Wire::Fixed => bad(what, "fixed-width value in a varint array"),
    }
}

fn unzig(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Running wrapping sums of zigzag deltas.
fn deltas(values: &[u64]) -> Vec<i64> {
    let mut current = 0i64;
    values
        .iter()
        .map(|&v| {
            current = current.wrapping_add(unzig(v));
            current
        })
        .collect()
}

fn blob_payload(blob: &[u8]) -> Vec<u8> {
    let mut data: Option<(u64, &[u8])> = None;
    let mut raw_size: Option<i32> = None;
    for (field, wire) in fields(blob, "Blob") {
        match field {
            1 | 3 | 7 => {
                if data.is_some() {
                    bad("Blob", "more than one payload field");
                }
                data = Some((field, bytes(wire, "Blob payload")));
            }
            2 => raw_size = Some(scalar(wire, "Blob.raw_size") as i32),
            4..=6 => bad("Blob", format!("unsupported compression field {field}")),
            _ => {}
        }
    }
    let (field, payload) = data.unwrap_or_else(|| bad("Blob", "no payload field"));
    let out = match field {
        1 => payload.to_vec(),
        3 => {
            let mut out = Vec::new();
            ZlibDecoder::new(payload)
                .read_to_end(&mut out)
                .unwrap_or_else(|e| bad("Blob zlib payload", e));
            out
        }
        _ => zstd::stream::decode_all(payload).unwrap_or_else(|e| bad("Blob zstd payload", e)),
    };
    if let Some(declared) = raw_size
        && i64::from(declared) != out.len() as i64
    {
        bad(
            "Blob",
            format!("raw_size {declared} but the payload is {} bytes", out.len()),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// PrimitiveBlock
// ---------------------------------------------------------------------------

struct Block {
    strings: Vec<String>,
    granularity: i64,
    date_granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl Block {
    #[track_caller]
    fn string(&self, index: i64, what: &str) -> String {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.strings.get(i))
            .cloned()
            .unwrap_or_else(|| {
                bad(
                    what,
                    format!(
                        "string index {index} outside a table of {}",
                        self.strings.len()
                    ),
                )
            })
    }

    fn nano(&self, offset: i64, value: i64) -> i64 {
        offset.wrapping_add(self.granularity.wrapping_mul(value))
    }

    fn tags(&self, keys: &[u64], vals: &[u64], what: &str) -> Vec<(String, String)> {
        if keys.len() != vals.len() {
            bad(
                what,
                format!("{} tag keys but {} values", keys.len(), vals.len()),
            );
        }
        keys.iter()
            .zip(vals)
            .map(|(&k, &v)| {
                (
                    self.string(i64::from(k as u32), what),
                    self.string(i64::from(v as u32), what),
                )
            })
            .collect()
    }
}

fn decode_block(payload: &[u8], out: &mut DecodedPbf) {
    let mut block = Block {
        strings: Vec::new(),
        granularity: 100,
        date_granularity: 1000,
        lat_offset: 0,
        lon_offset: 0,
    };
    let mut groups = Vec::new();
    for (field, wire) in fields(payload, "PrimitiveBlock") {
        match field {
            1 => {
                for (f, w) in fields(bytes(wire, "StringTable"), "StringTable") {
                    if f == 1 {
                        let s = std::str::from_utf8(bytes(w, "StringTable entry"))
                            .unwrap_or_else(|e| bad("StringTable entry", e));
                        block.strings.push(s.to_owned());
                    }
                }
            }
            2 => groups.push(bytes(wire, "PrimitiveGroup")),
            17 => block.granularity = i64::from(scalar(wire, "granularity") as i32),
            18 => block.date_granularity = i64::from(scalar(wire, "date_granularity") as i32),
            19 => block.lat_offset = scalar(wire, "lat_offset") as i64,
            20 => block.lon_offset = scalar(wire, "lon_offset") as i64,
            _ => {}
        }
    }
    for group in groups {
        let mut dense = Vec::new();
        for (field, wire) in fields(group, "PrimitiveGroup") {
            match field {
                1 => out.nodes.push(decode_node(&block, bytes(wire, "Node"))),
                2 => dense.extend_from_slice(bytes(wire, "DenseNodes")),
                3 => out.ways.push(decode_way(&block, bytes(wire, "Way"))),
                4 => out
                    .relations
                    .push(decode_relation(&block, bytes(wire, "Relation"))),
                _ => {}
            }
        }
        decode_dense(&block, &dense, out);
    }
}

/// The common fields of a node, way or relation message.
#[derive(Default)]
struct Common {
    keys: Vec<u64>,
    vals: Vec<u64>,
    info: Vec<u8>,
}

impl Common {
    /// Take fields 2, 3 and 4; false for any other field.
    fn take(&mut self, field: u64, wire: Wire<'_>, what: &str) -> bool {
        match field {
            2 => repeated(&mut self.keys, wire, what),
            3 => repeated(&mut self.vals, wire, what),
            4 => self.info.extend_from_slice(bytes(wire, what)),
            _ => return false,
        }
        true
    }
}

fn plain_meta(block: &Block, info: &[u8]) -> Option<NormalizedMeta> {
    let mut meta = NormalizedMeta::default();
    for (field, wire) in fields(info, "Info") {
        match field {
            1 => {
                let version = scalar(wire, "Info.version") as i32;
                meta.version = (version >= 1).then_some(version);
            }
            2 => {
                meta.milli_timestamp = Some(
                    (scalar(wire, "Info.timestamp") as i64).wrapping_mul(block.date_granularity),
                );
            }
            3 => {
                let changeset = scalar(wire, "Info.changeset") as i64;
                meta.changeset = (changeset != -1).then_some(changeset);
            }
            4 => meta.uid = Some(scalar(wire, "Info.uid") as i32),
            5 => {
                let sid = scalar(wire, "Info.user_sid") as i32;
                meta.user = Some(block.string(i64::from(sid), "Info.user_sid"));
            }
            6 => meta.visible = Some(scalar(wire, "Info.visible") != 0),
            _ => {}
        }
    }
    meta.into_claimed()
}

fn decode_node(block: &Block, msg: &[u8]) -> NormalizedNode {
    let mut common = Common::default();
    let (mut id, mut lat, mut lon) = (None, None, None);
    for (field, wire) in fields(msg, "Node") {
        if common.take(field, wire, "Node") {
            continue;
        }
        match field {
            1 => id = Some(unzig(scalar(wire, "Node.id"))),
            8 => lat = Some(unzig(scalar(wire, "Node.lat"))),
            9 => lon = Some(unzig(scalar(wire, "Node.lon"))),
            _ => {}
        }
    }
    let id = id.unwrap_or_else(|| bad("Node", "no id"));
    NormalizedNode {
        id,
        nano_lat: block.nano(
            block.lat_offset,
            lat.unwrap_or_else(|| bad("Node", "no lat")),
        ),
        nano_lon: block.nano(
            block.lon_offset,
            lon.unwrap_or_else(|| bad("Node", "no lon")),
        ),
        tags: block.tags(&common.keys, &common.vals, "Node tags"),
        meta: plain_meta(block, &common.info),
    }
}

fn decode_way(block: &Block, msg: &[u8]) -> NormalizedWay {
    let mut common = Common::default();
    let mut id = None;
    let (mut refs, mut lats, mut lons) = (Vec::new(), Vec::new(), Vec::new());
    for (field, wire) in fields(msg, "Way") {
        if common.take(field, wire, "Way") {
            continue;
        }
        match field {
            1 => id = Some(scalar(wire, "Way.id") as i64),
            8 => repeated(&mut refs, wire, "Way.refs"),
            9 => repeated(&mut lats, wire, "Way.lat"),
            10 => repeated(&mut lons, wire, "Way.lon"),
            _ => {}
        }
    }
    let id = id.unwrap_or_else(|| bad("Way", "no id"));
    let refs = deltas(&refs);
    let locations = if lats.is_empty() && lons.is_empty() {
        None
    } else {
        if lats.len() != refs.len() || lons.len() != refs.len() {
            bad(
                "Way",
                format!(
                    "way {id}: {} refs, {} lats, {} lons",
                    refs.len(),
                    lats.len(),
                    lons.len()
                ),
            );
        }
        Some(
            deltas(&lats)
                .into_iter()
                .zip(deltas(&lons))
                .map(|(lat, lon)| {
                    (
                        block.nano(block.lat_offset, lat),
                        block.nano(block.lon_offset, lon),
                    )
                })
                .collect(),
        )
    };
    NormalizedWay {
        id,
        refs,
        locations,
        tags: block.tags(&common.keys, &common.vals, "Way tags"),
        meta: plain_meta(block, &common.info),
    }
}

fn decode_relation(block: &Block, msg: &[u8]) -> NormalizedRelation {
    let mut common = Common::default();
    let mut id = None;
    let (mut roles, mut memids, mut types) = (Vec::new(), Vec::new(), Vec::new());
    for (field, wire) in fields(msg, "Relation") {
        if common.take(field, wire, "Relation") {
            continue;
        }
        match field {
            1 => id = Some(scalar(wire, "Relation.id") as i64),
            8 => repeated(&mut roles, wire, "Relation.roles_sid"),
            9 => repeated(&mut memids, wire, "Relation.memids"),
            10 => repeated(&mut types, wire, "Relation.types"),
            _ => {}
        }
    }
    let id = id.unwrap_or_else(|| bad("Relation", "no id"));
    if roles.len() != memids.len() || types.len() != memids.len() {
        bad(
            "Relation",
            format!(
                "relation {id}: {} roles, {} memids, {} types",
                roles.len(),
                memids.len(),
                types.len()
            ),
        );
    }
    let members = deltas(&memids)
        .into_iter()
        .zip(roles.iter().zip(&types))
        .map(|(ref_id, (&role, &kind))| NormalizedMember {
            member_type: match kind as i32 {
                0 => MemberKind::Node,
                1 => MemberKind::Way,
                2 => MemberKind::Relation,
                other => MemberKind::Unknown(other),
            },
            ref_id,
            role: block.string(i64::from(role as i32), "Relation role"),
        })
        .collect();
    NormalizedRelation {
        id,
        members,
        tags: block.tags(&common.keys, &common.vals, "Relation tags"),
        meta: plain_meta(block, &common.info),
    }
}

// ---------------------------------------------------------------------------
// DenseNodes
// ---------------------------------------------------------------------------

/// The DenseInfo columns, each empty when the group lacks it.
#[derive(Default)]
struct DenseColumns {
    version: Vec<u64>,
    timestamp: Vec<u64>,
    changeset: Vec<u64>,
    uid: Vec<u64>,
    user_sid: Vec<u64>,
    visible: Vec<u64>,
}

fn decode_dense(block: &Block, msg: &[u8], out: &mut DecodedPbf) {
    let (mut ids, mut lats, mut lons, mut keys_vals) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut info = Vec::new();
    for (field, wire) in fields(msg, "DenseNodes") {
        match field {
            1 => repeated(&mut ids, wire, "DenseNodes.id"),
            5 => info.extend_from_slice(bytes(wire, "DenseInfo")),
            8 => repeated(&mut lats, wire, "DenseNodes.lat"),
            9 => repeated(&mut lons, wire, "DenseNodes.lon"),
            10 => repeated(&mut keys_vals, wire, "DenseNodes.keys_vals"),
            _ => {}
        }
    }
    let mut columns = DenseColumns::default();
    for (field, wire) in fields(&info, "DenseInfo") {
        match field {
            1 => repeated(&mut columns.version, wire, "DenseInfo.version"),
            2 => repeated(&mut columns.timestamp, wire, "DenseInfo.timestamp"),
            3 => repeated(&mut columns.changeset, wire, "DenseInfo.changeset"),
            4 => repeated(&mut columns.uid, wire, "DenseInfo.uid"),
            5 => repeated(&mut columns.user_sid, wire, "DenseInfo.user_sid"),
            6 => repeated(&mut columns.visible, wire, "DenseInfo.visible"),
            _ => {}
        }
    }
    let n = ids.len();
    if lats.len() != n || lons.len() != n {
        bad(
            "DenseNodes",
            format!("{n} ids, {} lats, {} lons", lats.len(), lons.len()),
        );
    }
    for (name, column) in [
        ("version", &columns.version),
        ("timestamp", &columns.timestamp),
        ("changeset", &columns.changeset),
        ("uid", &columns.uid),
        ("user_sid", &columns.user_sid),
        ("visible", &columns.visible),
    ] {
        if !column.is_empty() && column.len() != n {
            bad(
                "DenseInfo",
                format!("{name} column of {} for {n} nodes", column.len()),
            );
        }
    }
    let tags = dense_tags(block, &keys_vals, n);
    let ids = deltas(&ids);
    let lats = deltas(&lats);
    let lons = deltas(&lons);
    let timestamps = deltas(&columns.timestamp);
    let changesets = deltas(&columns.changeset);
    let uids = deltas(&columns.uid);
    let user_sids = deltas(&columns.user_sid);
    for (i, node_tags) in tags.into_iter().enumerate() {
        // An absent column reads 0, `visible` true.
        let version = columns.version.get(i).map_or(0, |&v| v as i32);
        let timestamp = timestamps.get(i).copied().unwrap_or(0);
        let changeset = changesets.get(i).copied().unwrap_or(0);
        let uid = uids.get(i).map_or(0, |&v| v as i32);
        let user_sid = user_sids.get(i).map_or(0, |&v| v as i32);
        let visible = columns.visible.get(i).is_none_or(|&v| v != 0);
        let milli = timestamp.wrapping_mul(block.date_granularity);
        let meta = if version >= 1 {
            NormalizedMeta {
                version: Some(version),
                milli_timestamp: (!columns.timestamp.is_empty()).then_some(milli),
                changeset: (!columns.changeset.is_empty()).then_some(if changeset == -1 {
                    0
                } else {
                    changeset
                }),
                uid: (!columns.uid.is_empty()).then_some(uid),
                user: (!columns.user_sid.is_empty())
                    .then(|| block.string(i64::from(user_sid), "DenseInfo user")),
                visible: Some(visible),
            }
        } else {
            NormalizedMeta {
                version: None,
                milli_timestamp: (milli != 0).then_some(milli),
                changeset: (changeset > 0).then_some(changeset),
                uid: (uid > 0).then_some(uid),
                user: (user_sid != 0)
                    .then(|| block.string(i64::from(user_sid), "DenseInfo user"))
                    .filter(|user| !user.is_empty()),
                visible: (!visible).then_some(false),
            }
        };
        out.nodes.push(NormalizedNode {
            id: ids[i],
            nano_lat: block.nano(block.lat_offset, lats[i]),
            nano_lon: block.nano(block.lon_offset, lons[i]),
            tags: node_tags,
            meta: meta.into_claimed(),
        });
    }
}

/// Split `keys_vals` into one tag list per node: a 0 in key position ends a
/// node's run; an empty array leaves every node untagged, and the last run
/// may omit its 0.
fn dense_tags(block: &Block, keys_vals: &[u64], n: usize) -> Vec<Vec<(String, String)>> {
    if keys_vals.is_empty() {
        return vec![Vec::new(); n];
    }
    let mut out = Vec::with_capacity(n);
    let mut rest = keys_vals;
    for i in 0..n {
        if rest.is_empty() {
            bad(
                "DenseNodes.keys_vals",
                format!("runs end at node {i} of {n}"),
            );
        }
        let mut tags = Vec::new();
        loop {
            match rest {
                [] => break,
                [0, tail @ ..] => {
                    rest = tail;
                    break;
                }
                [key, value, tail @ ..] => {
                    tags.push((
                        block.string(i64::from(*key as u32), "dense tag key"),
                        block.string(i64::from(*value as u32), "dense tag value"),
                    ));
                    rest = tail;
                }
                [_] => bad("DenseNodes.keys_vals", "a key without a value"),
            }
        }
        out.push(tags);
    }
    if !rest.is_empty() {
        bad(
            "DenseNodes.keys_vals",
            format!("{} values past the last node", rest.len()),
        );
    }
    out
}
