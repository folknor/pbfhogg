//! Dense runs borrow the packed columns. Only the first delta needs a new
//! origin when a run starts a PrimitiveGroup. Read through the columns once
//! for checked coordinates (and the exact bbox), sentinel detection and the
//! source origins for later runs. We don't materialize a row or encode the
//! interior of a non-string column.

use protohoggr::{
    encode_bytes_field, encode_tag, encode_varint, zigzag_encode_32, zigzag_encode_64,
};

use crate::blob_meta::BlobBbox;
use crate::read::wire::{Cursor, WIRE_LEN, WireDenseInfo, WireDenseNodes};

use super::{
    DiffHit, ELEMENT_CAP, ELEMENT_OVERHEAD, EditSplice, Result, Selection, VARINT_BYTES,
    WireOutput, use_builder,
};

impl WireOutput<'_, '_> {
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub(super) fn dense(&mut self, data: &[u8], selection: &mut Selection<'_>) -> Result<()> {
        let mut columns = Dense::new(data)?;
        let mut run = Run::default();
        let mut id = 0i64;
        let mut ids = Cursor::new(columns.ids);
        let mut versions = Cursor::new(columns.versions);
        while !ids.is_empty() {
            id = id.wrapping_add(ids.read_sint64()?);
            let version_start = versions.position();
            let version = if versions.is_empty() {
                None
            } else {
                Some(i32::try_from(versions.read_varint_i64()?).map_err(|_| use_builder())?)
            };
            let (creates, hit) = selection.row(id)?;
            if !creates.is_empty() || hit != DiffHit::None {
                self.write_run(&mut columns, &mut run)?;
                self.add_edits(creates)?;
                selection.check_version(id, hit, version)?;
                if hit != DiffHit::None {
                    columns.skip_row()?;
                    self.apply_hit(hit)?;
                    continue;
                }
            }
            if run.count == 0 {
                self.ready_for_base()?;
            }
            if version == Some(-1) {
                // The element writer normalizes the Osmosis sentinel.
                return Err(use_builder());
            }
            run.push(id, ids.position(), version_start, versions.position());
            if self.count + run.count == ELEMENT_CAP {
                self.write_run(&mut columns, &mut run)?;
            }
        }
        self.write_run(&mut columns, &mut run)
    }

    fn write_run(&mut self, columns: &mut Dense<'_>, run: &mut Run) -> Result<()> {
        if run.count == 0 {
            return Ok(());
        }
        self.add_bytes(run.count * ELEMENT_OVERHEAD)?;
        let (source, slots) = (self.source, &mut self.slots);
        let mut map = |sid, key| {
            let target = slots.source_sid(source, sid)?;
            if key {
                slots.keys.insert(target);
            }
            Ok(target)
        };
        let refs = columns.write_run(
            run,
            &mut map,
            &mut self.fields.dense,
            &mut self.groups,
            &mut self.bbox,
        )?;
        self.add_bytes(refs * VARINT_BYTES)?;
        self.observe(run.min_id, run.count);
        self.observe(run.max_id, 0);
        self.stats.base_nodes += run.count as u64;
        run.count = 0;
        Ok(())
    }
}

#[derive(Default)]
struct Run {
    count: usize,
    first: i64,
    min_id: i64,
    max_id: i64,
    after_first_id: usize,
    end_id: usize,
    start_version: usize,
    end_version: usize,
}

impl Run {
    fn push(&mut self, id: i64, end: usize, version_start: usize, version_end: usize) {
        if self.count == 0 {
            self.first = id;
            self.after_first_id = end;
            self.start_version = version_start;
            self.min_id = id;
            self.max_id = id;
        }
        self.count += 1;
        self.min_id = self.min_id.min(id);
        self.max_id = self.max_id.max(id);
        self.end_id = end;
        self.end_version = version_end;
    }
}

/// An advancing column. Origins are maintained in the source's wrapping
/// domain, including sint32 for UID and user.
struct Column<'a> {
    data: &'a [u8],
    cursor: Cursor<'a>,
    value: i64,
}

impl<'a> Column<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            cursor: Cursor::new(data),
            value: 0,
        }
    }

    fn next<const NARROW: bool>(&mut self) -> Result<i64> {
        if !self.data.is_empty() {
            self.value = if NARROW {
                let prev = i32::try_from(self.value).map_err(|_| use_builder())?;
                i64::from(prev.wrapping_add(self.cursor.read_sint32()?))
            } else {
                self.value.wrapping_add(self.cursor.read_sint64()?)
            };
        }
        Ok(self.value)
    }

    /// Append an absolute first value and the original packed interior.
    /// The check also visits the first row (for example to extend the bbox).
    fn copy<const NARROW: bool>(
        &mut self,
        count: usize,
        mut check: impl FnMut(i64) -> Result<()>,
        field: u32,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let first = self.next::<NARROW>()?;
        check(first)?;
        let interior = self.cursor.position();
        for _ in 1..count {
            check(self.next::<NARROW>()?)?;
        }
        let first = if NARROW {
            u64::from(zigzag_encode_32(
                i32::try_from(first).map_err(|_| use_builder())?,
            ))
        } else {
            zigzag_encode_64(first)
        };
        write_rebased(
            out,
            field,
            first,
            &self.data[interior..self.cursor.position()],
        );
        Ok(())
    }
}

fn write_rebased(out: &mut Vec<u8>, field: u32, first: u64, rest: &[u8]) {
    encode_tag(out, field, WIRE_LEN);
    let width = (64 - first.leading_zeros()).div_ceil(7).max(1);
    encode_varint(out, u64::from(width) + rest.len() as u64);
    encode_varint(out, first);
    out.extend_from_slice(rest);
}

struct Dense<'a> {
    ids: &'a [u8],
    lat: Column<'a>,
    lon: Column<'a>,
    versions: &'a [u8],
    timestamp: Column<'a>,
    changeset: Column<'a>,
    uid: Column<'a>,
    user: Column<'a>,
    visible: &'a [u8],
    visibility: Cursor<'a>,
    tags: &'a [u8],
    tag: Cursor<'a>,
}

impl<'a> Dense<'a> {
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn new(data: &'a [u8]) -> Result<Self> {
        let dense = WireDenseNodes::parse(data)?;
        let info = dense.info_data.map(WireDenseInfo::parse).transpose()?;
        let field = |f: fn(WireDenseInfo<'a>) -> &'a [u8]| info.map_or(&[][..], f);
        let versions = field(|i| i.version_data);
        let visible = field(|i| i.visible_data);
        Ok(Self {
            ids: dense.id_data,
            lat: Column::new(dense.lat_data),
            lon: Column::new(dense.lon_data),
            versions,
            timestamp: Column::new(field(|i| i.timestamp_data)),
            changeset: Column::new(field(|i| i.changeset_data)),
            uid: Column::new(field(|i| i.uid_data)),
            user: Column::new(field(|i| i.user_sid_data)),
            visible,
            visibility: Cursor::new(visible),
            tags: dense.keys_vals_data,
            tag: Cursor::new(dense.keys_vals_data),
        })
    }

    fn skip_row(&mut self) -> Result<()> {
        self.lat.next::<false>()?;
        self.lon.next::<false>()?;
        self.timestamp.next::<false>()?;
        self.changeset.next::<false>()?;
        self.uid.next::<true>()?;
        self.user.next::<true>()?;
        if !self.visible.is_empty() {
            self.visibility.read_varint()?;
        }
        if !self.tags.is_empty() {
            while self.tag.read_varint()? != 0 {
                self.tag.read_varint()?;
            }
        }
        Ok(())
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn write_run(
        &mut self,
        run: &Run,
        map: &mut impl FnMut(u32, bool) -> Result<u32>,
        buffers: &mut Buffers,
        output: &mut Vec<u8>,
        bbox: &mut Option<BlobBbox>,
    ) -> Result<usize> {
        let Buffers {
            group,
            dense,
            info,
            strings,
        } = buffers;
        dense.clear();
        info.clear();
        write_rebased(
            dense,
            1,
            zigzag_encode_64(run.first),
            &self.ids[run.after_first_id..run.end_id],
        );
        let refs = self.write_info(run, map, info, strings)?;
        encode_bytes_field(dense, 5, info);
        let (min_lat, max_lat) = copy_coords(&mut self.lat, run.count, 8, dense)?;
        let (min_lon, max_lon) = copy_coords(&mut self.lon, run.count, 9, dense)?;
        extend_bbox(bbox, BlobBbox::new(min_lat, max_lat, min_lon, max_lon));
        let tags = self.write_tags(run.count, map, strings)?;
        encode_bytes_field(dense, 10, tags.0);
        group.clear();
        encode_bytes_field(group, 2, dense);
        encode_bytes_field(output, 2, group);
        Ok(refs + tags.1)
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn write_info(
        &mut self,
        run: &Run,
        map: &mut impl FnMut(u32, bool) -> Result<u32>,
        info: &mut Vec<u8>,
        strings: &mut Vec<u8>,
    ) -> Result<usize> {
        let count = run.count;
        let versions = &self.versions[run.start_version..run.end_version];
        encode_bytes_field(info, 1, versions);
        self.timestamp.copy::<false>(count, |_| Ok(()), 2, info)?;
        self.changeset.copy::<false>(
            count,
            |v| if v == -1 { Err(use_builder()) } else { Ok(()) },
            3,
            info,
        )?;
        self.uid.copy::<true>(count, |_| Ok(()), 4, info)?;

        let mut refs = 0;
        if !self.user.data.is_empty() {
            strings.clear();
            let mut last = 0i32;
            for _ in 0..count {
                let start = self.user.cursor.position();
                let previous = self.user.value;
                let source = self.user.next::<true>()?;
                let sid = u32::try_from(source).map_err(|_| use_builder())?;
                let target = i32::try_from(map(sid, false)?).map_err(|_| use_builder())?;
                let delta = target.wrapping_sub(last);
                let original = &self.user.data[start..self.user.cursor.position()];
                if i64::from(delta) == source - previous {
                    strings.extend_from_slice(original);
                } else {
                    encode_varint(strings, u64::from(zigzag_encode_32(delta)));
                }
                last = target;
            }
            encode_bytes_field(info, 5, strings);
            refs = count;
        }

        let visible = take_column(&mut self.visibility, self.visible, count)?;
        if visible.is_empty()
            && !versions.is_empty()
            && [&self.timestamp, &self.changeset, &self.uid, &self.user]
                .iter()
                .all(|column| !column.data.is_empty())
        {
            // The element writer supplies implicit true for a complete set
            // of the five core columns.
            encode_tag(info, 6, WIRE_LEN);
            encode_varint(info, count as u64);
            info.extend(std::iter::repeat_n(1, count));
        } else {
            encode_bytes_field(info, 6, visible);
        }
        Ok(refs)
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn write_tags<'b>(
        &'b mut self,
        count: usize,
        map: &mut impl FnMut(u32, bool) -> Result<u32>,
        strings: &'b mut Vec<u8>,
    ) -> Result<(&'b [u8], usize)> {
        if self.tags.is_empty() {
            return Ok((&[], 0));
        }
        strings.clear();
        let start = self.tag.position();
        let mut copied = start;
        let mut refs = 0;
        for _ in 0..count {
            loop {
                let at = self.tag.position();
                let sid = self.tag.read_varint_u32()?;
                if sid == 0 {
                    break;
                }
                let value = map(sid, true)?;
                if sid != value {
                    strings.extend_from_slice(&self.tags[copied..at]);
                    encode_varint(strings, u64::from(value));
                    copied = self.tag.position();
                }
                let at = self.tag.position();
                let sid = self.tag.read_varint_u32()?;
                let value = map(sid, false)?;
                if sid != value {
                    strings.extend_from_slice(&self.tags[copied..at]);
                    encode_varint(strings, u64::from(value));
                    copied = self.tag.position();
                }
                refs += 2;
            }
        }
        let end = self.tag.position();
        if copied == start {
            return Ok((&self.tags[start..end], refs));
        }
        strings.extend_from_slice(&self.tags[copied..end]);
        Ok((strings, refs))
    }
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn copy_coords(
    column: &mut Column<'_>,
    count: usize,
    field: u32,
    out: &mut Vec<u8>,
) -> Result<(i32, i32)> {
    let (mut min, mut max) = (i32::MAX, i32::MIN);
    column.copy::<false>(
        count,
        |coord| {
            let coord = i32::try_from(coord).map_err(|_| use_builder())?;
            min = min.min(coord);
            max = max.max(coord);
            Ok(())
        },
        field,
        out,
    )?;
    Ok((min, max))
}

pub(super) fn extend_bbox(bbox: &mut Option<BlobBbox>, next: BlobBbox) {
    let current = bbox.get_or_insert(next);
    current.min_lat = current.min_lat.min(next.min_lat);
    current.max_lat = current.max_lat.max(next.max_lat);
    current.min_lon = current.min_lon.min(next.min_lon);
    current.max_lon = current.max_lon.max(next.max_lon);
}

fn take_column<'a>(cursor: &mut Cursor<'_>, data: &'a [u8], count: usize) -> Result<&'a [u8]> {
    if data.is_empty() {
        return Ok(data);
    }
    let start = cursor.position();
    for _ in 0..count {
        cursor.read_varint()?;
    }
    Ok(&data[start..cursor.position()])
}

#[derive(Default)]
pub(super) struct Buffers {
    group: Vec<u8>,
    dense: Vec<u8>,
    info: Vec<u8>,
    strings: Vec<u8>,
}

/// Reuse the same column encoder for dense groups from the OSC builder,
/// which already carry the output framing and the normalized metadata.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn write_edits(
    data: &[u8],
    map: &mut impl FnMut(u32, bool) -> Result<u32>,
    buffers: &mut Buffers,
    splice: &mut EditSplice<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mut dense = Dense::new(data)?;
    let mut ids = Cursor::new(dense.ids);
    let mut versions = Cursor::new(dense.versions);
    let mut run = Run::default();
    let mut id = 0i64;
    let mut anchor = None;
    let mut bbox = None;
    while !ids.is_empty() {
        let after = splice.next()?;
        if anchor != Some(after) {
            if run.count != 0 {
                dense.write_run(&run, map, buffers, out, &mut bbox)?;
                run.count = 0;
            }
            splice.append_until(after, out)?;
            anchor = Some(after);
        }
        id = id.wrapping_add(ids.read_sint64()?);
        let version = versions.position();
        if !versions.is_empty() {
            versions.read_varint()?;
        }
        run.push(id, ids.position(), version, versions.position());
    }
    if run.count != 0 {
        dense.write_run(&run, map, buffers, out, &mut bbox)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rebased_coordinate_column_keeps_the_interior_wire_bytes() {
        let mut column = Column::new(&[12, 4, 0x83, 0]);
        assert_eq!(column.next::<false>().unwrap(), 6);
        let mut out = Vec::new();
        assert_eq!(copy_coords(&mut column, 2, 8, &mut out).unwrap(), (6, 8));
        assert_eq!(out, [0x42, 3, 16, 0x83, 0]);
    }
}
