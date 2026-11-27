//! Byte-equivalence proof for the sweep's simplest wire shape: a raw block
//! containing one dense node group without tags or metadata. The original
//! frame can be reused if the sweep would keep every node in place and the
//! builder, size policy and framer would produce exactly these bytes.

use crate::ElementKind;
use crate::blob_meta::{BlobBbox, BlobIndex};
use crate::osm_id::osm_id_cmp;
use crate::read::blob_wire::{MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE};
use crate::read::wire_schema::{blob, blob_header, primitive_block};
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT, zigzag_decode_64};

pub(super) fn prove(frame: &[u8], claim: &BlobIndex) -> Option<u64> {
    let count = usize::try_from(claim.count).ok()?;
    if claim.kind != ElementKind::Node
        || !(1..=crate::tunables::DEFAULT_ELEMENT_CAP.get()).contains(&count)
    {
        return None;
    }
    let (header, body) = super::super::frame_parts(frame).ok()?;
    if header.len() as u64 >= MAX_BLOB_HEADER_SIZE || body.len() as u64 >= MAX_BLOB_DATASIZE {
        return None;
    }
    let mut envelope = Cursor::new(body);
    let mut block = Cursor::new(field(&mut envelope, blob::RAW)?);
    if !envelope.is_empty() || field(&mut block, primitive_block::STRING_TABLE)? != [0x0a, 0x00] {
        return None; // StringTable with only the empty slot zero
    }
    let mut group = Cursor::new(field(&mut block, 2)?);
    let mut dense = Cursor::new(field(&mut group, 2)?);
    if !block.is_empty() || !group.is_empty() {
        return None;
    }
    let (min_id, max_id) = id_range(field(&mut dense, 1)?, count)?;
    let (min_lat, max_lat) = coord_range(field(&mut dense, 8)?, count)?;
    let (min_lon, max_lon) = coord_range(field(&mut dense, 9)?, count)?;
    if !dense.is_empty() || (min_id, max_id) != (claim.min_id, claim.max_id) {
        return None;
    }
    let index = BlobIndex {
        bbox: Some(BlobBbox::new(min_lat, max_lat, min_lon, max_lon)),
        ..*claim
    };
    // The writer orders these three fields and emits no tagdata or
    // WayMembers payload for this block. Require the complete header.
    let mut header = Cursor::new(header);
    if field(&mut header, blob_header::TYPE)? != b"OSMData"
        || field(&mut header, blob_header::INDEXDATA)? != index.serialize()
        || canonical_varint(&mut header)? != u64::from((blob_header::DATASIZE << 3) | WIRE_VARINT)
        || canonical_varint(&mut header)? != body.len() as u64
        || !header.is_empty()
    {
        return None;
    }
    Some(claim.count)
}

/// Compare the wire spelling as well as the value. Overlong varints and
/// extra fields can decode to the same elements with different output bytes.
fn canonical_varint(cursor: &mut Cursor<'_>) -> Option<u64> {
    let start = cursor.position();
    let value = cursor.read_varint().ok()?;
    (cursor.position() - start == varint_width(value)).then_some(value)
}

fn varint_width(value: u64) -> usize {
    (u64::BITS - value.leading_zeros()).max(1).div_ceil(7) as usize
}

fn field<'a>(cursor: &mut Cursor<'a>, number: u32) -> Option<&'a [u8]> {
    if canonical_varint(cursor)? != u64::from((number << 3) | WIRE_LEN) {
        return None;
    }
    let start = cursor.position();
    let bytes = cursor.read_len_delimited().ok()?;
    let width = cursor.position() - start - bytes.len();
    (width == varint_width(bytes.len() as u64)).then_some(bytes)
}

fn id_range(bytes: &[u8], count: usize) -> Option<(i64, i64)> {
    let mut cursor = Cursor::new(bytes);
    let mut id = zigzag_decode_64(canonical_varint(&mut cursor)?);
    let (mut min, mut max) = (id, id);
    for _ in 1..count {
        let next = id.checked_add(zigzag_decode_64(canonical_varint(&mut cursor)?))?;
        if !osm_id_cmp(id, next).is_lt() {
            return None;
        }
        id = next;
        min = min.min(id);
        max = max.max(id);
    }
    cursor.is_empty().then_some((min, max))
}

fn coord_range(bytes: &[u8], count: usize) -> Option<(i32, i32)> {
    let mut cursor = Cursor::new(bytes);
    let mut coord = 0i64;
    let (mut min, mut max) = (i32::MAX, i32::MIN);
    for _ in 0..count {
        coord = coord.checked_add(zigzag_decode_64(canonical_varint(&mut cursor)?))?;
        let valid = i32::try_from(coord).ok()?;
        min = min.min(valid);
        max = max.max(valid);
    }
    cursor.is_empty().then_some((min, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_builder::BlockBuilder;
    use crate::write::metrics::WriterMetrics;
    use crate::writer::{Compression, frame_blob_pipelined};

    #[test]
    fn proof_requires_the_sweep_output_header_and_payload() {
        let mut builder = BlockBuilder::new();
        builder.add_node(1, 17, -21, [], None);
        builder.add_node(3, -12, 83, [], None);
        let block = builder.take_owned().expect("encode").expect("block");
        let indexdata = block.index.serialize();
        let frame = |indexdata: &[u8], tagdata| {
            frame_blob_pipelined(
                &block.bytes,
                &Compression::None,
                Some(indexdata),
                tagdata,
                None,
                &WriterMetrics::default(),
            )
            .expect("frame block")
        };
        assert_eq!(prove(&frame(&indexdata, None), &block.index), Some(2));
        // The scanning writer emits a present empty tag index for the same
        // payload. Copying it would retain a field the sweep leaves out.
        let (_, tags) =
            crate::blob_meta::scan_block_ids_and_tags(&block.bytes).expect("scan block");
        let tagdata = tags.expect("tag key set").serialize().expect("tag index");
        assert_eq!(
            prove(&frame(&indexdata, Some(&tagdata)), &block.index),
            None
        );
        let wider = BlobIndex {
            bbox: Some(BlobBbox::new(-100, 100, -100, 100)),
            ..block.index
        };
        assert_eq!(prove(&frame(&wider.serialize(), None), &wider), None);
        let false_count = BlobIndex {
            count: 1,
            ..block.index
        };
        assert_eq!(
            prove(&frame(&false_count.serialize(), None), &false_count),
            None
        );
    }
}
