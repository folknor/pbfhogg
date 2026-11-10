//! `cat --repair`: validate every data blob and rewrite the ones refused for
//! a shape `cat` can turn into a valid one.
//!
//! The one shape repaired today is a repeated data-bearing `Info` (field 4 of
//! a `Node`, `Way` or `Relation`) or `DenseInfo` (field 5 of a `DenseNodes`),
//! which every reader refuses (`take_message` in `read/wire.rs`, recorded in
//! DEVIATIONS.md). Protobuf defines the message such an encoding carries: the
//! occurrences merge, a scalar set in a later occurrence overriding an earlier
//! one, one set only earlier surviving, and a repeated field (every
//! `DenseInfo` column) concatenating. [`merge_repeated_metadata`] rewrites
//! the block into the canonical encoding of that merged message, and the
//! rewritten block is then validated as every block is: whatever it still
//! fails on (merged `DenseInfo` columns longer than the group, a refusal of
//! another kind) is refused. Nothing is guessed, so nothing is announced
//! beyond the merge itself.
//!
//! Without `--clean` the route is [`repair_passthrough`]: every data blob is
//! decompressed and validated on a frame pool. An accepted blob goes where
//! plain `cat` sends it (an indexed one copied whole with its claim, which
//! this route does not check: `--reindex` owns indexdata; an unindexed one
//! reframed with scanned indexdata, or rebuilt when no single index describes
//! it). A repaired blob's rewritten payload is compressed under the run's
//! compression and framed under its own `BlobHeader`, every field kept but
//! indexdata, tagdata and datasize, which are replaced: a merge changes
//! metadata only, so the way count, order and refs a `WayMembers-v1` field 5
//! describes are those of the input. With `--clean` the repair rides on the
//! clean pass's decode (`BlockBuffer::block_rescuing`), which re-encodes
//! every block anyway.
//!
//! A well-formed file pays only for the validating decode the flag asks
//! for; plain `cat`, `--clean` and `--type` without `--repair` run exactly
//! as before.

use crate::blob::decompress_blob_data_into;
use crate::block_builder::OwnedBlock;
use crate::error::{Error, Result};
use crate::read::block::{NoProjection, validate_in_place};
use crate::read::frame::RawBlobFrame;
use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT};
use crate::writer::Compression;

use super::super::output_header::Declared;

/// The context a rewritten block's refusal carries.
pub(crate) const MERGED_CONTEXT: &str =
    "after merging a repeated Info or DenseInfo message as protobuf merges it";

/// What [`merge_repeated_metadata`] merged in one block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Merged {
    /// Plain nodes, ways and relations whose `Info` was merged.
    pub(crate) elements: u64,
    /// Dense groups whose `DenseInfo` was merged.
    pub(crate) dense_groups: u64,
}

/// What a repair run merged, over every blob it rewrote.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RepairTally {
    pub(crate) blobs: u64,
    pub(crate) merged: Merged,
}

impl RepairTally {
    pub(crate) fn add(&mut self, merged: Merged) {
        self.blobs += 1;
        self.merged.elements += merged.elements;
        self.merged.dense_groups += merged.dense_groups;
    }

    /// The run's one note, after it succeeded.
    pub(crate) fn note(&self, host: &crate::host::Entered<'_>) {
        if self.blobs == 0 {
            return;
        }
        crate::host::diag!(
            host.diagnostics,
            Note,
            "cat.merged_repeated_metadata",
            "merged repeated metadata in {} elements and {} dense groups across {} input \
             blobs: each repeated Info or DenseInfo message was merged as protobuf merges a \
             repeated message field (a field set in a later occurrence overrides an earlier \
             one, DenseInfo columns concatenate), and the blob's payload rewritten",
            self.merged.elements,
            self.merged.dense_groups,
            self.blobs
        );
    }
}

/// The rescue `BlockBuffer::block_rescuing` takes on the clean pass: a
/// refusal of a repeated `Info` or `DenseInfo` is rewritten, any other
/// refusal stands.
pub(crate) fn rescue(err: &Error, bytes: &[u8]) -> Result<Option<(Vec<u8>, Merged)>> {
    if !crate::read::wire::is_repeated_metadata(err) {
        return Ok(None);
    }
    merge_repeated_metadata(bytes)
}

// ---------------------------------------------------------------------------
// The rewrite
// ---------------------------------------------------------------------------

/// Rewrite a decompressed `PrimitiveBlock` so that no element carries more
/// than one non-empty `Info` and no dense group more than one non-empty
/// `DenseInfo`, each such run replaced by the canonical encoding of the
/// message protobuf merges it into. `None` when the block holds no such
/// element (the refusal that sent it here stands).
///
/// Every byte the merge does not touch is copied: the block's other fields,
/// its string table, every other group field, and every field of a merged
/// element but its `Info` occurrences. Elements and dense groups with at most
/// one non-empty occurrence are copied whole, even when a neighbour is
/// merged, so a lone `DenseInfo` with a column split across segments stays
/// as it is (and refused, as it is everywhere).
///
/// - `Info`: the non-empty bodies concatenated in order, at the position of
///   the first occurrence; empty occurrences (which merge to nothing) are
///   dropped. Concatenating the bodies is protobuf's merge for a message of
///   scalars: `WireInfo::parse` keeps the last value of each field, and its
///   Osmosis `-1` mapping runs on the merged value, so a later explicit `-1`
///   clears an earlier version as protobuf says it does.
/// - `DenseInfo`: one packed segment per column (fields 1 to 6), holding
///   every occurrence's segments of that column in order, a single unpacked
///   value included; delta decoding then runs across the joined column, as
///   it does over the merged message. Fields of other numbers or wire types
///   are copied in occurrence order after the columns.
///
/// Each occurrence is checked on its own first: a body must frame cleanly to
/// its end, and a column segment must hold whole varints. Concatenation
/// would otherwise let a truncated earlier body swallow the start of the
/// next one into a field that frames. Column lengths are not checked per
/// occurrence: that invariant belongs to the merged message, which block
/// validation checks afterwards.
pub(crate) fn merge_repeated_metadata(block: &[u8]) -> Result<Option<(Vec<u8>, Merged)>> {
    let mut merged = Merged::default();
    let mut out = Vec::with_capacity(block.len());
    let mut cursor = Cursor::new(block);
    loop {
        let start = cursor.position();
        let Some((field, wire_type)) = cursor.read_tag()? else {
            break;
        };
        if (field, wire_type) == (2, WIRE_LEN) {
            let group = cursor.read_len_delimited()?;
            match merge_group(group, &mut merged)? {
                Some(rewritten) => put_len_field(&mut out, 2, &rewritten),
                None => out.extend_from_slice(&block[start..cursor.position()]),
            }
        } else {
            cursor.skip_field(wire_type)?;
            out.extend_from_slice(&block[start..cursor.position()]);
        }
    }
    Ok((merged != Merged::default()).then_some((out, merged)))
}

/// One `PrimitiveGroup`, rewritten when any element or its dense message
/// merged.
fn merge_group(group: &[u8], merged: &mut Merged) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(group.len());
    let mut changed = false;
    let mut cursor = Cursor::new(group);
    loop {
        let start = cursor.position();
        let Some((field, wire_type)) = cursor.read_tag()? else {
            break;
        };
        let rewritten = match (field, wire_type) {
            // Node, Way, Relation: `info` is field 4 of each.
            (1 | 3 | 4, WIRE_LEN) => {
                let message = cursor.read_len_delimited()?;
                let rewritten = merge_info(message)?;
                if rewritten.is_some() {
                    merged.elements += 1;
                }
                rewritten
            }
            (2, WIRE_LEN) => {
                let message = cursor.read_len_delimited()?;
                let rewritten = merge_dense_info(message)?;
                if rewritten.is_some() {
                    merged.dense_groups += 1;
                }
                rewritten
            }
            _ => {
                cursor.skip_field(wire_type)?;
                None
            }
        };
        match rewritten {
            Some(message) => {
                put_len_field(&mut out, field, &message);
                changed = true;
            }
            None => out.extend_from_slice(&group[start..cursor.position()]),
        }
    }
    Ok(changed.then_some(out))
}

/// The occurrences of length-delimited field `target` in `message`, as
/// `(field start, field end, body)`.
fn occurrences(message: &[u8], target: u32) -> Result<Vec<(usize, usize, &[u8])>> {
    let mut found = Vec::new();
    let mut cursor = Cursor::new(message);
    loop {
        let start = cursor.position();
        let Some((field, wire_type)) = cursor.read_tag()? else {
            break;
        };
        if (field, wire_type) == (target, WIRE_LEN) {
            let body = cursor.read_len_delimited()?;
            found.push((start, cursor.position(), body));
        } else {
            cursor.skip_field(wire_type)?;
        }
    }
    Ok(found)
}

/// `message` with its occurrences of field `target` replaced by `merged`,
/// written at the first occurrence's position.
fn replace_occurrences(
    message: &[u8],
    found: &[(usize, usize, &[u8])],
    target: u32,
    merged: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len());
    let mut copied = 0;
    for (index, &(start, end, _)) in found.iter().enumerate() {
        out.extend_from_slice(&message[copied..start]);
        if index == 0 {
            put_len_field(&mut out, target, merged);
        }
        copied = end;
    }
    out.extend_from_slice(&message[copied..]);
    out
}

/// A plain element whose `Info` (field 4) occurs non-empty more than once,
/// rewritten with one merged `Info`; `None` otherwise.
fn merge_info(element: &[u8]) -> Result<Option<Vec<u8>>> {
    let found = occurrences(element, 4)?;
    if found.iter().filter(|(_, _, body)| !body.is_empty()).count() < 2 {
        return Ok(None);
    }
    let mut merged = Vec::new();
    for &(_, _, body) in &found {
        check_framing(body)?;
        merged.extend_from_slice(body);
    }
    Ok(Some(replace_occurrences(element, &found, 4, &merged)))
}

/// A `DenseNodes` whose `DenseInfo` (field 5) occurs non-empty more than
/// once, rewritten with one merged `DenseInfo`; `None` otherwise.
fn merge_dense_info(dense: &[u8]) -> Result<Option<Vec<u8>>> {
    let found = occurrences(dense, 5)?;
    if found.iter().filter(|(_, _, body)| !body.is_empty()).count() < 2 {
        return Ok(None);
    }
    // Columns 1 to 6: version, timestamp, changeset, uid, user_sid, visible.
    let mut columns: [Vec<u8>; 6] = Default::default();
    let mut other = Vec::new();
    for &(_, _, body) in &found {
        let mut cursor = Cursor::new(body);
        loop {
            let start = cursor.position();
            let Some((field, wire_type)) = cursor.read_tag()? else {
                break;
            };
            let column = usize::try_from(field)
                .ok()
                .and_then(|f| f.checked_sub(1))
                .and_then(|i| columns.get_mut(i));
            match (column, wire_type) {
                (Some(column), WIRE_LEN) => {
                    let segment = cursor.read_len_delimited()?;
                    crate::read::block::packed_varint_count(segment)?;
                    column.extend_from_slice(segment);
                }
                (Some(column), WIRE_VARINT) => {
                    column.extend_from_slice(cursor.read_raw_field(WIRE_VARINT)?);
                }
                _ => {
                    cursor.skip_field(wire_type)?;
                    other.extend_from_slice(&body[start..cursor.position()]);
                }
            }
        }
    }
    let mut merged = Vec::new();
    for (field, column) in (1u32..).zip(&columns) {
        if !column.is_empty() {
            put_len_field(&mut merged, field, column);
        }
    }
    merged.extend_from_slice(&other);
    Ok(Some(replace_occurrences(dense, &found, 5, &merged)))
}

/// Refuse a message body that does not frame cleanly to its end.
fn check_framing(body: &[u8]) -> Result<()> {
    let mut cursor = Cursor::new(body);
    while let Some((_, wire_type)) = cursor.read_tag()? {
        cursor.skip_field(wire_type)?;
    }
    Ok(())
}

fn put_len_field(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    protohoggr::encode_bytes_field(out, field, bytes);
}

// ---------------------------------------------------------------------------
// The passthrough route
// ---------------------------------------------------------------------------

/// One frame-pool worker's scratch.
#[derive(Default)]
pub(super) struct RepairScratch {
    payload: Vec<u8>,
    st: Vec<(u32, u32)>,
    groups: Vec<(u32, u32)>,
    header_buf: Vec<u8>,
}

/// What became of one input data blob.
pub(super) enum Outcome {
    /// Copied whole, or reframed with scanned indexdata.
    Copied,
    /// Repaired into one blob: its rewritten payload, recompressed.
    Rewritten,
    /// Rebuilt into `blocks` single-kind blocks holding `elements` elements
    /// (none: omitted), because no single index describes it.
    Rebuilt { blocks: usize, elements: u64 },
}

/// One input data blob's output frames, in order, what became of it, and
/// what was merged in it if it was repaired.
pub(super) struct Repaired {
    pub(super) frames: Vec<Vec<u8>>,
    pub(super) outcome: Outcome,
    pub(super) merged: Option<Merged>,
}

/// Validate one OSMData frame and send it where `cat --repair` sends it
/// (the module doc says where). Runs on a frame-pool worker.
pub(super) fn repair_frame(
    mut frame: RawBlobFrame,
    scratch: &mut RepairScratch,
    declared: Declared,
    compression: Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<Repaired> {
    scratch.payload.clear();
    decompress_blob_data_into(frame.blob_bytes(), &mut scratch.payload)?;
    let refused = validate_in_place(
        &scratch.payload,
        &mut scratch.st,
        &mut scratch.groups,
        None,
        |_, _| NoProjection,
    )
    .err();
    let Some(err) = refused else {
        if frame.index.is_some() {
            return Ok(Repaired {
                frames: vec![std::mem::take(&mut frame.frame_bytes)],
                outcome: Outcome::Copied,
                merged: None,
            });
        }
        let payload = std::mem::take(&mut scratch.payload);
        return unindexed(
            &frame,
            payload,
            None,
            declared,
            compression,
            metrics,
            scratch,
        );
    };
    let Some((payload, merged)) = rescue(&err, &scratch.payload)? else {
        return Err(err);
    };
    validate_in_place(
        &payload,
        &mut scratch.st,
        &mut scratch.groups,
        None,
        |_, _| NoProjection,
    )
    .map_err(|e| e.context(MERGED_CONTEXT))?;
    unindexed(
        &frame,
        payload,
        Some(merged),
        declared,
        compression,
        metrics,
        scratch,
    )
}

/// The frames of a validated block plain `cat` would not copy whole: an
/// unindexed blob (`merged` is `None`), or a repaired one, indexed or not.
/// Either is scanned for its index; one no single index describes is
/// rebuilt, as plain `cat` rebuilds it. An unindexed way blob under a
/// declared `WayMembers-v1` is refused, as plain `cat` refuses it: its
/// header carries no field 5 to keep.
fn unindexed(
    frame: &RawBlobFrame,
    payload: Vec<u8>,
    merged: Option<Merged>,
    declared: Declared,
    compression: Compression,
    metrics: &crate::write::metrics::WriterMetrics,
    scratch: &mut RepairScratch,
) -> Result<Repaired> {
    let (index, tagdata) = crate::blob_meta::scan_block_ids_and_tags(&payload)?;
    let Some(index) = index else {
        let (blocks, elements) = super::reencode_unclassifiable(payload, declared)?;
        let count = blocks.len();
        return Ok(Repaired {
            frames: frame_blocks(blocks, compression, metrics)?,
            outcome: Outcome::Rebuilt {
                blocks: count,
                elements,
            },
            merged,
        });
    };
    if frame.index.is_none() && declared.way_members && index.kind == crate::ElementKind::Way {
        return Err(declared.broken_by_unindexed_way_blob("adding its indexdata"));
    }
    let tagdata = tagdata
        .as_ref()
        .and_then(crate::blob_meta::TagIndex::serialize);
    let bytes = if merged.is_some() {
        // A new payload: compressed afresh, framed under the input header.
        let fresh =
            crate::writer::frame_blob_pipelined(&payload, &compression, None, None, None, metrics)?;
        let blob = blob_of_frame(&fresh)?;
        super::reindex::reframe_replacing_body(
            frame.header_bytes(),
            blob,
            &index.serialize(),
            tagdata.as_deref(),
            &mut scratch.header_buf,
        )?
        .0
    } else {
        crate::writer::reframe_raw_with_index(
            frame.blob_bytes(),
            &index.serialize(),
            tagdata.as_deref(),
            metrics,
        )?
    };
    Ok(Repaired {
        frames: vec![bytes],
        outcome: if merged.is_some() {
            Outcome::Rewritten
        } else {
            Outcome::Copied
        },
        merged,
    })
}

/// The `Blob` message of a frame `[4-byte BE header length][BlobHeader][Blob]`
/// this module's writer call just built.
fn blob_of_frame(frame: &[u8]) -> Result<&[u8]> {
    let prefix: [u8; 4] = frame
        .get(..4)
        .and_then(|p| p.try_into().ok())
        .ok_or_else(|| Error::internal("a framed blob starts with its header length"))?;
    let start = 4 + u32::from_be_bytes(prefix) as usize;
    frame
        .get(start..)
        .ok_or_else(|| Error::internal("a framed blob holds the header it declares"))
}

/// Frame rebuilt blocks under the run's compression, each with its own
/// indexdata and tagdata.
fn frame_blocks(
    blocks: Vec<OwnedBlock>,
    compression: Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> Result<Vec<Vec<u8>>> {
    blocks
        .into_iter()
        .map(|block| {
            crate::writer::frame_blob_pipelined(
                &block.bytes,
                &compression,
                Some(&block.index.serialize()),
                block.tagdata.as_deref(),
                block.way_members.as_deref(),
                metrics,
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push(u8::try_from(v & 0x7f).unwrap() | 0x80);
            v >>= 7;
        }
        out.push(u8::try_from(v).unwrap());
    }

    fn field(n: u32, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_len_field(&mut out, n, bytes);
        out
    }

    fn vfield(n: u32, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(u64::from(n) << 3, &mut out);
        varint(v, &mut out);
        out
    }

    fn block_of(group: &[u8]) -> Vec<u8> {
        [field(1, &field(1, b"")), field(2, group)].concat()
    }

    fn only_group(block: &[u8]) -> Vec<u8> {
        let found = occurrences(block, 2).unwrap();
        assert_eq!(found.len(), 1);
        found[0].2.to_vec()
    }

    #[test]
    fn info_bodies_concatenate_at_the_first_position() {
        let a = vfield(1, 3); // version 3
        let b = [vfield(2, 100), vfield(1, 4)].concat(); // timestamp, version 4
        let node = [
            vfield(1, 2),
            field(4, &a),
            vfield(8, 10),
            field(4, &[]),
            field(4, &b),
            vfield(9, 20),
        ]
        .concat();
        let block = block_of(&field(1, &node));
        let (out, merged) = merge_repeated_metadata(&block).unwrap().unwrap();
        assert_eq!(
            merged,
            Merged {
                elements: 1,
                dense_groups: 0
            }
        );
        let expected = [
            vfield(1, 2),
            field(4, &[a, b].concat()),
            vfield(8, 10),
            vfield(9, 20),
        ]
        .concat();
        assert_eq!(only_group(&out), field(1, &expected));
        // The merged block is one every reader accepts, version 4 winning.
        let parsed = crate::read::wire::WireNode::parse(&expected).unwrap();
        let info = crate::read::wire::WireInfo::parse(parsed.info_data.unwrap()).unwrap();
        assert_eq!(info.version, Some(4));
        assert_eq!(info.timestamp, Some(100));
    }

    #[test]
    fn a_truncated_earlier_body_is_refused_not_swallowed() {
        // Unknown field 15, length 2, with no body: framed alone it is
        // truncated; concatenated it would eat `version = 1`.
        let a = [0x7a, 0x02];
        let b = vfield(1, 1);
        let way = [vfield(1, 1), field(4, &a), field(4, &b)].concat();
        let block = block_of(&field(3, &way));
        assert!(merge_repeated_metadata(&block).is_err());
    }

    #[test]
    fn dense_info_columns_join_per_column() {
        let mut v1 = Vec::new();
        varint(1, &mut v1);
        let a = [field(1, &v1), field(2, &[0x02])].concat(); // version [1], ts [1]
        let b = [vfield(1, 2), field(2, &[0x04]), field(9, b"x")].concat(); // version 2 unpacked
        let dense = [field(1, &[0x02, 0x02]), field(5, &a), field(5, &b)].concat();
        let block = block_of(&field(2, &dense));
        let (out, merged) = merge_repeated_metadata(&block).unwrap().unwrap();
        assert_eq!(merged.dense_groups, 1);
        let info = [
            field(1, &[0x01, 0x02]),
            field(2, &[0x02, 0x04]),
            field(9, b"x"),
        ]
        .concat();
        let expected = [field(1, &[0x02, 0x02]), field(5, &info)].concat();
        assert_eq!(only_group(&out), field(2, &expected));
    }

    #[test]
    fn a_dense_column_ending_mid_varint_is_refused() {
        let a = field(1, &[0x81]);
        let b = field(1, &[0x01]);
        let dense = [field(1, &[0x02, 0x02]), field(5, &a), field(5, &b)].concat();
        assert!(merge_repeated_metadata(&block_of(&field(2, &dense))).is_err());
    }

    #[test]
    fn a_block_without_a_repeat_is_left_alone() {
        let node = [vfield(1, 2), field(4, &vfield(1, 1)), field(4, &[])].concat();
        assert_eq!(
            merge_repeated_metadata(&block_of(&field(1, &node))).unwrap(),
            None
        );
    }

    #[test]
    fn untouched_neighbours_are_copied_byte_for_byte() {
        // A lone DenseInfo with a split column stays split beside a merged
        // node in the same group.
        let lone = [field(1, &[0x01]), field(1, &[0x01])].concat();
        let dense = [field(1, &[0x02]), field(5, &lone)].concat();
        let node = [
            vfield(1, 2),
            field(4, &vfield(1, 1)),
            field(4, &vfield(1, 2)),
        ]
        .concat();
        let group = [field(2, &dense), field(1, &node)].concat();
        let (out, merged) = merge_repeated_metadata(&block_of(&group)).unwrap().unwrap();
        assert_eq!(merged.elements, 1);
        assert_eq!(merged.dense_groups, 0);
        let rewritten = only_group(&out);
        assert!(rewritten.starts_with(&field(2, &dense)));
    }
}
