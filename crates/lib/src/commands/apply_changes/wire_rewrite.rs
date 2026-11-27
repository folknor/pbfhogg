//! Speculative wire rewrite of an indexed candidate. The input has passed
//! the shared structural validator. Until the claim walk and every frame
//! succeed, the result stays private and the original writer owns refusals.

mod dense;
mod message;

use std::borrow::Cow;
use std::ops::Range;

use protohoggr::{encode_bytes_field, encode_bytes_field_always, encode_tag, encode_varint};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ElementKind;
use crate::blob_meta::{BlobBbox, BlobIndex, encode_tag_index};
use crate::block_builder::BlockBuilder;
use crate::commands::split::{Limits, varint_len};
use crate::error::Error;
use crate::osc::sorted::SortedDiff;
use crate::read::block::ValidatedBlock;
use crate::read::wire::{Cursor, WIRE_LEN, WireInfo, WireRelation, WireWay};
use crate::writer::Compression;

use super::Result;
use super::classify::{BlobWalk, DiffHit, KindSeeks, blob_walk};
use super::element_writes::{RefreshTally, add_osc_node, add_osc_relation, add_osc_way};
use super::loc_mode::LocMode;
use super::refresh::StaleProbe;
use super::rewrite_block::{
    ELEMENT_CAP, ELEMENT_OVERHEAD, FIELD_OVERHEAD, RewriteOutput, VARINT_BYTES, osc_bytes,
};
use super::stats::MergeStats;

use message::{FieldBuffers, WayFields};

fn use_builder() -> Error {
    Error::internal("apply-changes: wire rewrite needs the element writer")
}

#[derive(Clone, Copy)]
pub(super) struct WireInputs<'a> {
    pub(super) claim: &'a BlobIndex,
    pub(super) diff: &'a SortedDiff,
    pub(super) loc: LocMode<'a>,
    pub(super) probe: &'a StaleProbe,
    pub(super) compression: &'a Compression,
    pub(super) metrics: &'a crate::write::metrics::WriterMetrics,
}

pub(super) struct WireRewrite {
    pub(super) first: i64,
    pub(super) output: RewriteOutput,
    pub(super) forced: bool,
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn try_rewrite(
    block: &ValidatedBlock<'_>,
    inputs: WireInputs<'_>,
    bb: &mut BlockBuilder,
    refresh_only: bool,
) -> Option<WireRewrite> {
    let fields = block.scan().fields;
    if (fields.granularity, fields.lat_offset, fields.lon_offset) != (100, 0, 0)
        || i64::from(fields.date_granularity) != crate::owned::OUTPUT_DATE_GRANULARITY
        || block.string(0) != Some("")
    {
        return None;
    }
    // Every failure here is provisional. Replay through the original path
    // to report its refusal or use its overflow recovery.
    let result = if refresh_only {
        refresh_block(block, inputs, bb)
    } else {
        rewrite(block, inputs, bb)
    };
    result.unwrap_or(None)
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn rewrite<'a>(
    block: &'a ValidatedBlock<'_>,
    inputs: WireInputs<'a>,
    bb: &mut BlockBuilder,
) -> Result<Option<WireRewrite>> {
    let source_table_bytes = source_table_size(block);
    let sure = Limits::new(*inputs.compression).sure();
    if source_table_bytes > sure {
        return Ok(None);
    }
    let kind = inputs.claim.kind;
    let mut selection = Selection::new(&inputs);
    let mut output = WireOutput::new(inputs, bb, block, source_table_bytes, sure);
    let mut cursor = Cursor::new(block.bytes());
    while let Some((field, wire)) = cursor.read_tag()? {
        if (field, wire) != (2, WIRE_LEN) {
            cursor.skip_field(wire)?;
            continue;
        }
        rewrite_group(
            cursor.read_len_delimited()?,
            kind,
            &mut output,
            &mut selection,
        )?;
    }
    output.add_edits(selection.inline.clone())?;
    let first = selection.walk.finish()?.ok_or_else(use_builder)?;
    if !selection.matched && !output.had_edits {
        return Ok(None);
    }
    output.flush_frame()?;
    Ok(Some(WireRewrite {
        first,
        output: output.finish(),
        forced: !selection.matched,
    }))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn source_table_size(block: &ValidatedBlock<'_>) -> usize {
    (0..block.string_count())
        .map(|i| block.string(i).map_or(0, |s| string_field_len(s.len())))
        .sum()
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn rewrite_group(
    data: &[u8],
    kind: ElementKind,
    output: &mut WireOutput<'_, '_>,
    selection: &mut Selection<'_>,
) -> Result<()> {
    let mut group = Cursor::new(data);
    while let Some((field, wire)) = group.read_tag()? {
        match (kind, field, wire) {
            (ElementKind::Node, 2, WIRE_LEN) => {
                output.dense(group.read_len_delimited()?, selection)?;
            }
            (ElementKind::Way, 3, WIRE_LEN) | (ElementKind::Relation, 4, WIRE_LEN) => {
                output.ordinary(group.read_len_delimited()?, selection)?;
            }
            _ => return Err(use_builder()),
        }
    }
    Ok(())
}

fn message_field(kind: ElementKind) -> u32 {
    match kind {
        ElementKind::Node => 1,
        ElementKind::Way => 3,
        ElementKind::Relation => 4,
    }
}

struct Selection<'a> {
    walk: BlobWalk<'a>,
    seeks: KindSeeks,
    inline: Range<usize>,
    diff: &'a SortedDiff,
    claim: &'a BlobIndex,
    matched: bool,
}

impl<'a> Selection<'a> {
    fn new(inputs: &WireInputs<'a>) -> Self {
        Self {
            walk: blob_walk(Some(inputs.claim)),
            seeks: KindSeeks::default(),
            inline: 0..0,
            diff: inputs.diff,
            claim: inputs.claim,
            matched: false,
        }
    }

    fn row(&mut self, id: i64) -> Result<(Range<usize>, DiffHit)> {
        let kind = self.claim.kind;
        if self.walk.first().is_none() {
            self.find_inline_edits(id);
        }
        self.walk.observe(id)?;
        let start = self.inline.start;
        let upserts = self.diff.upserts(kind);
        while !self.inline.is_empty()
            && crate::osm_id::osm_id_cmp(upserts[self.inline.start], id).is_lt()
        {
            self.inline.start += 1;
        }
        let creates = start..self.inline.start;
        if !self.inline.is_empty() && upserts[self.inline.start] == id {
            self.inline.start += 1;
        }
        let hit = self.seeks.find(self.diff, kind, id);
        self.matched |= hit != DiffHit::None;
        Ok((creates, hit))
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn find_inline_edits(&mut self, id: i64) {
        let last = crate::osm_id::blob_osm_last_key(self.claim.min_id, self.claim.max_id);
        self.inline = self.diff.upsert_range(self.claim.kind, id, last);
    }

    fn check_version(&self, id: i64, hit: DiffHit, version: Option<i32>) -> Result<()> {
        let index = match hit {
            DiffHit::Upserted(i) => Ok(i),
            DiffHit::Deleted(i) => Err(i),
            DiffHit::None => return Ok(()),
        };
        self.diff
            .check_base_version(self.claim.kind, id, index, version.filter(|&v| v >= 1))
    }
}

/// Each source slot is translated once per output batch. Two source slots
/// with the same contents stay distinct, while OSC strings are interned by
/// content. The reserved empty slot stays zero even if another slot is empty.
struct Slots<'a> {
    source: Vec<u32>,
    values: Vec<Cow<'a, [u8]>>,
    interned: FxHashMap<Cow<'a, [u8]>, u32>,
    keys: FxHashSet<u32>,
    table_len: usize,
}

impl<'a> Slots<'a> {
    const UNMAPPED: u32 = u32::MAX;

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn new(source_count: usize) -> Self {
        Self {
            source: vec![Self::UNMAPPED; source_count],
            values: vec![Cow::Borrowed(b"")],
            interned: FxHashMap::default(),
            keys: FxHashSet::default(),
            table_len: string_field_len(0),
        }
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn clear(&mut self) {
        self.source.fill(Self::UNMAPPED);
        self.values.truncate(1);
        self.interned.clear();
        self.keys.clear();
        self.table_len = string_field_len(0);
    }

    fn push(&mut self, value: Cow<'a, [u8]>) -> Result<u32> {
        let slot = u32::try_from(self.values.len()).map_err(|_| use_builder())?;
        self.table_len += string_field_len(value.len());
        self.interned.entry(value.clone()).or_insert(slot);
        self.values.push(value);
        Ok(slot)
    }

    fn source_sid(&mut self, source: &'a ValidatedBlock<'_>, index: u32) -> Result<u32> {
        if index == 0 {
            return Ok(0);
        }
        let slot = *self.source.get(index as usize).ok_or_else(use_builder)?;
        if slot != Self::UNMAPPED {
            return Ok(slot);
        }
        let value = source.string(index as usize).ok_or_else(use_builder)?;
        let slot = self.push(Cow::Borrowed(value.as_bytes()))?;
        self.source[index as usize] = slot;
        Ok(slot)
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn edit_table(&mut self, data: &[u8]) -> Result<Vec<u32>> {
        let mut table = Cursor::new(data);
        let mut map = Vec::new();
        while !table.is_empty() {
            if table.read_tag()? != Some((1, WIRE_LEN)) {
                return Err(use_builder());
            }
            let value = table.read_len_delimited()?;
            let sid = match (map.is_empty(), self.interned.get(value)) {
                (true, _) if value.is_empty() => 0,
                (true, _) => return Err(use_builder()),
                (false, Some(&sid)) => sid,
                (false, None) => self.push(Cow::Owned(value.to_vec()))?,
            };
            map.push(sid);
        }
        Ok(map)
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn write_table(&self, out: &mut Vec<u8>) {
        encode_tag(out, 1, WIRE_LEN);
        encode_varint(out, self.table_len as u64);
        for value in &self.values {
            encode_bytes_field_always(out, 1, value);
        }
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn tagdata(&self) -> Option<Vec<u8>> {
        if self.keys.is_empty() {
            return None;
        }
        let mut keys: Vec<&[u8]> = self
            .keys
            .iter()
            .map(|&sid| self.values[sid as usize].as_ref())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        encode_tag_index(keys.into_iter())
    }
}

fn string_field_len(bytes: usize) -> usize {
    1 + varint_len(bytes) + bytes
}

/// Refresh a blob that fits the builder's existing element cap in place
/// on the wire. Its default framing already represents the new locations.
/// Keep the source table and group layout, and copy every unaffected span.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn refresh_block<'a>(
    block: &'a ValidatedBlock<'_>,
    inputs: WireInputs<'a>,
    bb: &mut BlockBuilder,
) -> Result<Option<WireRewrite>> {
    if inputs.claim.kind != ElementKind::Way
        || block.scan().element_count > ELEMENT_CAP as u64
        || !matches!(inputs.loc, LocMode::Required(_))
    {
        return Ok(None);
    }
    let source = block.bytes();
    let sure = Limits::new(*inputs.compression).sure();
    // Includes the source table, framing fields, and every original group.
    let mut output = WireOutput::new(inputs, bb, block, source.len(), sure);
    let mut walk = blob_walk(Some(output.inputs.claim));
    let mut keys = FxHashSet::default();
    let mut cursor = Cursor::new(source);
    let mut group_out = Vec::new();
    while !cursor.is_empty() {
        let start = cursor.position();
        let (field, wire) = cursor.read_tag()?.ok_or_else(use_builder)?;
        if (field, wire) != (2, WIRE_LEN) {
            cursor.skip_field(wire)?;
            output
                .payload
                .extend_from_slice(&source[start..cursor.position()]);
            continue;
        }
        let group = cursor.read_len_delimited()?;
        let before = output.refresh.ways;
        group_out.clear();
        refresh_group(group, &mut output, &mut walk, &mut keys, &mut group_out)?;
        if output.refresh.ways == before {
            output
                .payload
                .extend_from_slice(&source[start..cursor.position()]);
        } else {
            encode_bytes_field_always(&mut output.payload, 2, &group_out);
        }
    }
    let first = walk.finish()?.ok_or_else(use_builder)?;
    if output.refresh.ways == 0 {
        return Ok(None);
    }
    let tagdata = refresh_tagdata(keys);
    let index = BlobIndex {
        bbox: None,
        ..*output.inputs.claim
    };
    output.frame(index, tagdata.as_deref())?;
    output.stats.base_ways = index.count;
    Ok(Some(WireRewrite {
        first,
        output: output.finish(),
        forced: false,
    }))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn refresh_group<'a>(
    group: &[u8],
    output: &mut WireOutput<'a, '_>,
    walk: &mut BlobWalk<'_>,
    keys: &mut FxHashSet<&'a [u8]>,
    group_out: &mut Vec<u8>,
) -> Result<()> {
    let block = output.source;
    let mut ways = Cursor::new(group);
    while !ways.is_empty() {
        let start = ways.position();
        if ways.read_tag()? != Some((3, WIRE_LEN)) {
            return Err(use_builder());
        }
        let data = ways.read_len_delimited()?;
        let way = WireWay::parse(data)?;
        if way.pins_data.is_some() {
            // The normal rewrite removes stale enrichment.
            return Err(use_builder());
        }
        walk.observe(way.id)?;
        output.add_bytes(ELEMENT_OVERHEAD + VARINT_BYTES * way.refs_data.len())?;
        output
            .fields
            .prepare_locations(&way, output.inputs.probe, &mut output.refresh)?;
        let changed = output.fields.replace_locations;
        let mut identity = |sid, key| {
            let value = block.string(sid as usize).ok_or_else(use_builder)?;
            if key {
                keys.insert(value.as_bytes());
            }
            Ok(sid)
        };
        let before_way = group_out.len();
        let refs = output.fields.write_message(
            data,
            ElementKind::Way,
            WayFields::RefreshOnly,
            &mut identity,
            group_out,
        )?;
        if !changed {
            group_out.truncate(before_way);
            group_out.extend_from_slice(&group[start..ways.position()]);
        }
        output.add_bytes(refs * VARINT_BYTES)?;
    }
    Ok(())
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn refresh_tagdata(keys: FxHashSet<&[u8]>) -> Option<Vec<u8>> {
    let mut keys: Vec<&[u8]> = keys.into_iter().collect();
    keys.sort_unstable();
    if keys.is_empty() {
        None
    } else {
        encode_tag_index(keys.into_iter())
    }
}

struct WireOutput<'a, 'b> {
    inputs: WireInputs<'a>,
    source: &'a ValidatedBlock<'a>,
    bb: &'b mut BlockBuilder,
    slots: Slots<'a>,
    edits: Vec<Edit>,
    merged: Vec<u8>,
    had_edits: bool,
    ordinary_group: Vec<u8>,
    groups: Vec<u8>,
    payload: Vec<u8>,
    fields: FieldBuffers,
    refresh: RefreshTally,
    stats: MergeStats,
    frames: Vec<(ElementKind, Vec<u8>)>,
    held_bytes: usize,
    frame_ns: u64,
    count: usize,
    min_id: i64,
    max_id: i64,
    bbox: Option<BlobBbox>,
    original_table_bytes: usize,
    previous_batch_bytes: usize,
    batch_bytes: usize,
    sure: usize,
}

impl<'a, 'b> WireOutput<'a, 'b> {
    fn new(
        inputs: WireInputs<'a>,
        bb: &'b mut BlockBuilder,
        source: &'a ValidatedBlock<'a>,
        original_table_bytes: usize,
        sure: usize,
    ) -> Self {
        Self {
            inputs,
            source,
            bb,
            slots: Slots::new(source.string_count()),
            edits: Vec::new(),
            merged: Vec::new(),
            had_edits: false,
            ordinary_group: Vec::new(),
            groups: Vec::new(),
            payload: Vec::new(),
            fields: FieldBuffers::default(),
            refresh: RefreshTally::default(),
            stats: MergeStats::new(),
            frames: Vec::new(),
            held_bytes: 0,
            frame_ns: 0,
            count: 0,
            min_id: i64::MAX,
            max_id: i64::MIN,
            bbox: None,
            original_table_bytes,
            previous_batch_bytes: 0,
            batch_bytes: 2 * FIELD_OVERHEAD + 2,
            sure,
        }
    }

    fn ordinary(&mut self, data: &[u8], selection: &mut Selection<'_>) -> Result<()> {
        let kind = self.inputs.claim.kind;
        let (id, info, way) = if kind == ElementKind::Way {
            let way = WireWay::parse(data)?;
            (way.id, way.info_data, Some(way))
        } else {
            let rel = WireRelation::parse(data)?;
            (rel.id, rel.info_data, None)
        };
        let (creates, hit) = selection.row(id)?;
        self.add_edits(creates)?;
        if hit != DiffHit::None {
            let version = info
                .map(WireInfo::parse)
                .transpose()?
                .and_then(|info| info.version);
            selection.check_version(id, hit, version)?;
            return self.apply_hit(hit);
        }
        self.ready_for_base()?;
        let located = match way {
            Some(way) if self.inputs.loc.for_way(id)?.is_some() => Some(way),
            _ => None,
        };
        // Two deltas between i32 coordinates need at most five bytes each.
        // Each reference occupies at least one byte in the source.
        let extra = located
            .as_ref()
            .filter(|way| self.inputs.probe.has_upserts() && !way.lat_data.is_empty())
            .map_or(0, |way| VARINT_BYTES * way.refs_data.len());
        self.add_bytes(ELEMENT_OVERHEAD + data.len() + extra)?;
        if let Some(way) = located {
            self.fields
                .prepare_locations(&way, self.inputs.probe, &mut self.refresh)?;
        }
        let (source, slots) = (self.source, &mut self.slots);
        let mut map = |sid, key| {
            let target = slots.source_sid(source, sid)?;
            if key {
                slots.keys.insert(target);
            }
            Ok(target)
        };
        let fields = if matches!(self.inputs.loc, LocMode::Off) {
            WayFields::RefsOnly
        } else {
            WayFields::Located
        };
        let refs =
            self.fields
                .write_message(data, kind, fields, &mut map, &mut self.ordinary_group)?;
        self.add_bytes(refs * VARINT_BYTES)?;
        self.observe(id, 1);
        if kind == ElementKind::Way {
            self.stats.base_ways += 1;
        } else {
            self.stats.base_relations += 1;
        }
        Ok(())
    }

    fn apply_hit(&mut self, hit: DiffHit) -> Result<()> {
        match hit {
            DiffHit::Upserted(i) => self.add_edit(i),
            DiffHit::Deleted(_) => {
                self.stats.deleted += 1;
                Ok(())
            }
            DiffHit::None => Ok(()),
        }
    }

    fn add_edits(&mut self, range: Range<usize>) -> Result<()> {
        for i in range {
            self.add_edit(i)?;
        }
        Ok(())
    }

    fn add_edit(&mut self, i: usize) -> Result<()> {
        if self.count == ELEMENT_CAP {
            self.flush_frame()?;
        }
        let kind = self.inputs.claim.kind;
        let diff = self.inputs.diff;
        self.add_bytes(osc_bytes(diff, kind, i, self.inputs.loc))?;
        self.observe(diff.upserts(kind)[i], 1);
        self.had_edits = true;
        let after = if kind == ElementKind::Node {
            self.groups.len()
        } else {
            self.ordinary_group.len()
        };
        self.edits.push(Edit { index: i, after });
        Ok(())
    }

    fn ready_for_base(&mut self) -> Result<()> {
        if self.count == ELEMENT_CAP {
            self.flush_frame()
        } else {
            Ok(())
        }
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<()> {
        self.batch_bytes = self.batch_bytes.saturating_add(bytes);
        // An original count-capped block could straddle adjacent compacted
        // batches after a seed-triggered split. Prove that even that block,
        // including the whole source table, is safe for the selected codec.
        if self
            .original_table_bytes
            .saturating_add(self.previous_batch_bytes)
            .saturating_add(self.batch_bytes)
            > self.sure
        {
            return Err(use_builder());
        }
        Ok(())
    }

    fn observe(&mut self, id: i64, count: usize) {
        self.count += count;
        self.min_id = self.min_id.min(id);
        self.max_id = self.max_id.max(id);
    }

    fn flush_edits(&mut self) -> Result<()> {
        if self.edits.is_empty() {
            return Ok(());
        }
        let result = self.encode_edits();
        if result.is_err() {
            // A failed OSC add can poison its builder. Start the original
            // path with a fresh one.
            *self.bb = BlockBuilder::new();
        }
        result
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn encode_edits(&mut self) -> Result<()> {
        let kind = self.inputs.claim.kind;
        let diff = self.inputs.diff;
        for edit in &self.edits {
            let i = edit.index;
            match kind {
                ElementKind::Node => {
                    add_osc_node(self.bb, &diff.node(i));
                    self.stats.diff_nodes += 1;
                }
                ElementKind::Way => {
                    let way = diff.way(i);
                    add_osc_way(self.bb, &way, self.inputs.loc.for_way(way.id())?);
                    self.stats.diff_ways += 1;
                }
                ElementKind::Relation => {
                    add_osc_relation(self.bb, &diff.relation(i));
                    self.stats.diff_relations += 1;
                }
            }
        }
        let existing = if kind == ElementKind::Node {
            &mut self.groups
        } else {
            &mut self.ordinary_group
        };
        self.merged.clear();
        let (fields, slots, out) = (&mut self.fields, &mut self.slots, &mut self.merged);
        let bbox = &mut self.bbox;
        let mut splice = EditSplice {
            source: existing.as_slice(),
            edits: self.edits.iter(),
            copied: 0,
        };
        self.bb
            .lend_encoded(|encoded| {
                write_edits(encoded.bytes, kind, fields, slots, &mut splice, out)?;
                if let Some(next) = encoded.index.bbox {
                    dense::extend_bbox(bbox, next);
                }
                splice.finish(out)
            })?
            .ok_or_else(use_builder)?;
        std::mem::swap(existing, &mut self.merged);
        self.edits.clear();
        Ok(())
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn flush_frame(&mut self) -> Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        self.flush_edits()?;
        let kind = self.inputs.claim.kind;
        let index = BlobIndex {
            kind,
            min_id: self.min_id,
            max_id: self.max_id,
            count: self.count as u64,
            bbox: self.bbox.take(),
        };
        self.assemble_payload();
        let tagdata = self.slots.tagdata();
        self.frame(index, tagdata.as_deref())?;
        self.groups.clear();
        self.ordinary_group.clear();
        self.slots.clear();
        self.count = 0;
        self.min_id = i64::MAX;
        self.max_id = i64::MIN;
        self.previous_batch_bytes = self.batch_bytes;
        self.batch_bytes = 2 * FIELD_OVERHEAD + 2;
        Ok(())
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn assemble_payload(&mut self) {
        self.payload.clear();
        self.slots.write_table(&mut self.payload);
        self.payload.extend_from_slice(&self.groups);
        encode_bytes_field(&mut self.payload, 2, &self.ordinary_group);
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn check_payload_size(&self) -> Result<()> {
        if self.payload.len() > self.sure {
            return Err(use_builder());
        }
        Ok(())
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn frame(&mut self, index: BlobIndex, tagdata: Option<&[u8]>) -> Result<()> {
        self.check_payload_size()?;
        let start = crate::counters::Stopwatch::start();
        let frame = crate::writer::frame_blob_pipelined(
            &self.payload,
            self.inputs.compression,
            Some(&serialize_indexdata(&index)),
            tagdata,
            None,
            self.inputs.metrics,
        )?;
        self.frame_ns += start.elapsed_ns();
        self.held_bytes += frame.len();
        if self.held_bytes > crate::tunables::APPLY_CHANGES_REWRITE_RETAIN_BYTES {
            return Err(use_builder());
        }
        self.frames.push((index.kind, frame));
        Ok(())
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn finish(self) -> RewriteOutput {
        RewriteOutput {
            frames: Some(self.frames),
            frame_ns: self.frame_ns,
            stats: self.stats,
            refresh: self.refresh,
        }
    }
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn serialize_indexdata(index: &BlobIndex) -> [u8; crate::blob_meta::INDEX_SIZE] {
    index.serialize()
}

/// The builder encodes the OSC records once per batch. Each offset is
/// between surviving messages (ordinary) or surviving groups (dense).
struct Edit {
    index: usize,
    after: usize,
}

struct EditSplice<'a> {
    source: &'a [u8],
    edits: std::slice::Iter<'a, Edit>,
    copied: usize,
}

impl EditSplice<'_> {
    fn next(&mut self) -> Result<usize> {
        Ok(self.edits.next().ok_or_else(use_builder)?.after)
    }

    fn append_until(&mut self, after: usize, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(
            self.source
                .get(self.copied..after)
                .ok_or_else(use_builder)?,
        );
        self.copied = after;
        Ok(())
    }

    fn finish(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.edits.len() != 0 {
            return Err(use_builder());
        }
        out.extend_from_slice(&self.source[self.copied..]);
        Ok(())
    }
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn write_edits(
    data: &[u8],
    kind: ElementKind,
    fields: &mut FieldBuffers,
    slots: &mut Slots<'_>,
    splice: &mut EditSplice<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut block = Cursor::new(data);
    if block.read_tag()? != Some((1, WIRE_LEN)) {
        return Err(use_builder());
    }
    let map = slots.edit_table(block.read_len_delimited()?)?;
    let mut translate = |sid: u32, key| {
        let target = *map.get(sid as usize).ok_or_else(use_builder)?;
        if key {
            slots.keys.insert(target);
        }
        Ok(target)
    };
    let mut plain = Vec::new();
    while !block.is_empty() {
        if block.read_tag()? != Some((2, WIRE_LEN)) {
            return Err(use_builder());
        }
        let mut group = Cursor::new(block.read_len_delimited()?);
        while !group.is_empty() {
            let (field, WIRE_LEN) = group.read_tag()?.ok_or_else(use_builder)? else {
                return Err(use_builder());
            };
            let message = group.read_len_delimited()?;
            if kind == ElementKind::Node && field == 2 {
                dense::write_edits(message, &mut translate, &mut fields.dense, splice, out)?;
                continue;
            }
            if field != message_field(kind) {
                return Err(use_builder());
            }
            let after = splice.next()?;
            splice.append_until(after, out)?;
            if kind == ElementKind::Node {
                plain.clear();
                fields.write_message(
                    message,
                    kind,
                    WayFields::Located,
                    &mut translate,
                    &mut plain,
                )?;
                encode_bytes_field(out, 2, &plain);
            } else {
                fields.write_message(message, kind, WayFields::Located, &mut translate, out)?;
            }
        }
    }
    Ok(())
}
