//! Edit ordinary message fields in place on the wire, copying the spans
//! around changed fields. The source block has already been validated.

use protohoggr::{encode_bytes_field, encode_bytes_field_always, encode_varint};

use crate::ElementKind;
use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT, WireWay};

use super::super::element_writes::RefreshTally;
use super::super::refresh::{CoordFraming, StaleProbe, evaluate_way, refreshed_locations};
use super::{Result, dense, message_field, use_builder};

#[derive(Default)]
pub(super) struct FieldBuffers {
    message: Vec<u8>,
    string_field: Vec<u8>,
    hits: Vec<(usize, (i32, i32))>,
    lats: Vec<u8>,
    lons: Vec<u8>,
    pub(super) replace_locations: bool,
    pub(super) dense: dense::Buffers,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WayFields {
    RefsOnly,
    Located,
    RefreshOnly,
}

impl FieldBuffers {
    pub(super) fn prepare_locations(
        &mut self,
        way: &WireWay<'_>,
        probe: &StaleProbe,
        tally: &mut RefreshTally,
    ) -> Result<()> {
        self.replace_locations = false;
        if probe.is_empty() {
            return Ok(());
        }
        let eval = evaluate_way(
            way.refs_data,
            way.lat_data,
            way.lon_data,
            CoordFraming::DEFAULT,
            probe,
            &mut self.hits,
        )?;
        tally.deleted_refs += eval.deleted_refs;
        if eval.stale_refs > 0 {
            let changed = refreshed_locations(
                eval.ref_count,
                way.lat_data,
                way.lon_data,
                CoordFraming::DEFAULT,
                &self.hits,
                &mut self.lats,
                &mut self.lons,
            )?;
            self.replace_locations = true;
            tally.ways += 1;
            tally.refs += changed;
        }
        Ok(())
    }

    pub(super) fn write_message(
        &mut self,
        data: &[u8],
        kind: ElementKind,
        way_fields: WayFields,
        map: &mut impl FnMut(u32, bool) -> Result<u32>,
        group: &mut Vec<u8>,
    ) -> Result<usize> {
        self.message.clear();
        let mut cur = Cursor::new(data);
        let mut copied = 0;
        let mut refs = 0;
        let mode = MessageMode {
            kind,
            way_fields,
            replace_locations: std::mem::take(&mut self.replace_locations),
        };
        while !cur.is_empty() {
            let start = cur.position();
            let tag = cur.read_tag()?.ok_or_else(use_builder)?;
            let (edit, count) = rewrite_field(&mut cur, tag, mode, map, &mut self.string_field)?;
            refs += count;
            if edit == FieldEdit::Keep {
                continue;
            }
            self.message.extend_from_slice(&data[copied..start]);
            if edit == FieldEdit::Replace {
                encode_bytes_field_always(&mut self.message, tag.0, &self.string_field);
            }
            copied = cur.position();
        }
        if copied == 0 {
            encode_bytes_field_always(group, message_field(kind), data);
        } else {
            self.message.extend_from_slice(&data[copied..]);
            if mode.replace_locations {
                encode_bytes_field(&mut self.message, 9, &self.lats);
                encode_bytes_field(&mut self.message, 10, &self.lons);
            }
            encode_bytes_field_always(group, message_field(kind), &self.message);
        }
        Ok(refs)
    }
}

#[derive(Clone, Copy)]
struct MessageMode {
    kind: ElementKind,
    way_fields: WayFields,
    replace_locations: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldEdit {
    Keep,
    Drop,
    Replace,
}

fn rewrite_field(
    cur: &mut Cursor<'_>,
    (field, wire): (u32, u32),
    mode: MessageMode,
    map: &mut impl FnMut(u32, bool) -> Result<u32>,
    scratch: &mut Vec<u8>,
) -> Result<(FieldEdit, usize)> {
    match (field, wire) {
        (2 | 3, WIRE_LEN | WIRE_VARINT) => remap_packed(cur, wire, field == 2, map, scratch),
        (8, WIRE_LEN | WIRE_VARINT) if mode.kind == ElementKind::Relation => {
            remap_packed(cur, wire, false, map, scratch)
        }
        (4, WIRE_LEN) => remap_info(
            cur.read_len_delimited()?,
            mode.way_fields == WayFields::RefreshOnly,
            map,
            scratch,
        ),
        (1, WIRE_VARINT) => keep_field(cur, wire),
        (8 | 9, WIRE_VARINT) if mode.kind == ElementKind::Node => keep_field(cur, wire),
        (8..=10, WIRE_LEN | WIRE_VARINT) if mode.kind == ElementKind::Relation => {
            keep_field(cur, wire)
        }
        (8, WIRE_LEN | WIRE_VARINT) if mode.kind == ElementKind::Way => keep_field(cur, wire),
        (9 | 10, WIRE_LEN | WIRE_VARINT) if mode.kind == ElementKind::Way => {
            if mode.way_fields == WayFields::RefsOnly || mode.replace_locations {
                cur.skip_field(wire)?;
                Ok((FieldEdit::Drop, 0))
            } else {
                keep_field(cur, wire)
            }
        }
        (20, WIRE_LEN)
            if mode.kind == ElementKind::Way && mode.way_fields != WayFields::RefreshOnly =>
        {
            cur.skip_field(wire)?;
            Ok((FieldEdit::Drop, 0))
        }
        _ => Err(use_builder()),
    }
}

fn keep_field(cur: &mut Cursor<'_>, wire: u32) -> Result<(FieldEdit, usize)> {
    cur.skip_field(wire)?;
    Ok((FieldEdit::Keep, 0))
}

fn remap_packed(
    cur: &mut Cursor<'_>,
    wire: u32,
    key: bool,
    map: &mut impl FnMut(u32, bool) -> Result<u32>,
    out: &mut Vec<u8>,
) -> Result<(FieldEdit, usize)> {
    let data = if wire == WIRE_LEN {
        cur.read_len_delimited()?
    } else {
        cur.read_raw_field(wire)?
    };
    out.clear();
    let mut indices = Cursor::new(data);
    let mut copied = 0;
    let mut refs = 0;
    while !indices.is_empty() {
        let start = indices.position();
        let sid = indices.read_varint_u32()?;
        let dest = map(sid, key)?;
        if sid != dest {
            out.extend_from_slice(&data[copied..start]);
            encode_varint(out, u64::from(dest));
            copied = indices.position();
        }
        refs += 1;
    }
    if copied == 0 {
        Ok((FieldEdit::Keep, refs))
    } else {
        out.extend_from_slice(&data[copied..]);
        Ok((FieldEdit::Replace, refs))
    }
}

/// Info uses the last varint user field. Compaction drops shadowed user
/// fields since their slots may have been removed. A refresh-only patch
/// keeps them because it keeps the original table.
fn remap_info(
    data: &[u8],
    keep_shadowed: bool,
    map: &mut impl FnMut(u32, bool) -> Result<u32>,
    out: &mut Vec<u8>,
) -> Result<(FieldEdit, usize)> {
    let mut cur = Cursor::new(data);
    let mut user = None;
    let mut count = 0;
    while !cur.is_empty() {
        let field_start = cur.position();
        let (field, wire) = cur.read_tag()?.ok_or_else(use_builder)?;
        if (field, wire) == (5, WIRE_VARINT) {
            let value_start = cur.position();
            let sid = cur.read_varint()?;
            user = Some((sid, field_start, value_start, cur.position()));
            count += 1;
        } else {
            cur.skip_field(wire)?;
        }
    }
    let Some((sid, field_start, start, end)) = user else {
        return Ok((FieldEdit::Keep, 0));
    };
    let sid = u32::try_from(sid).map_err(|_| use_builder())?;
    let dest = map(sid, false)?;
    let drop_shadowed = count > 1 && !keep_shadowed;
    if sid == dest && !drop_shadowed {
        return Ok((FieldEdit::Keep, 1));
    }
    out.clear();
    if drop_shadowed {
        copy_without_users(&data[..field_start], out)?;
        out.extend_from_slice(&data[field_start..start]);
    } else {
        out.extend_from_slice(&data[..start]);
    }
    encode_varint(out, u64::from(dest));
    out.extend_from_slice(&data[end..]);
    Ok((FieldEdit::Replace, 1))
}

fn copy_without_users(data: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let mut cursor = Cursor::new(data);
    while !cursor.is_empty() {
        let start = cursor.position();
        let (field, wire) = cursor.read_tag()?.ok_or_else(use_builder)?;
        cursor.skip_field(wire)?;
        if (field, wire) != (5, WIRE_VARINT) {
            out.extend_from_slice(&data[start..cursor.position()]);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::read::wire::{WireInfo, WireRelation};

    #[test]
    fn packed_remap_copies_the_non_string_spans() {
        let message = [
            0x08, 1, // ID
            0x10, 5, // unpacked key
            0x1a, 1, 6, // value
            0x22, 4, 0x28, 3, 0x28, 7, // only the last user is effective
            0x42, 1, 8, // role
            0x4a, 2, 0x82, 0, // original non-minimal member ID varint
            0x52, 1, 1, // way member
        ];
        let mut group = Vec::new();
        let mut visited = Vec::new();
        FieldBuffers::default()
            .write_message(
                &message,
                ElementKind::Relation,
                WayFields::Located,
                &mut |sid, key| {
                    visited.push((sid, key));
                    Ok(match sid {
                        5 => 2,
                        7 => 4,
                        8 => 1,
                        _ => sid,
                    })
                },
                &mut group,
            )
            .unwrap();

        let mut group = Cursor::new(&group);
        assert_eq!(group.read_tag().unwrap(), Some((4, WIRE_LEN)));
        let relation = WireRelation::parse(group.read_len_delimited().unwrap()).unwrap();
        assert_eq!(visited, [(5, true), (6, false), (7, false), (8, false)]);
        assert_eq!(relation.id, 1);
        assert_eq!(relation.keys_data, [2]);
        assert_eq!(relation.vals_data, [6]);
        assert_eq!(relation.roles_sid_data, [1]);
        assert_eq!(relation.memids_data, [0x82, 0]);
        let info = relation.info_data.unwrap();
        assert_eq!(WireInfo::user_sid_of(info).unwrap(), Some(4));
        assert_eq!(info, [0x28, 4]);
    }
}
