//! The re-encoding cut, for a block the first cut cannot partition.
//!
//! The first cut copies items whole beside the whole string table, so it
//! cannot cut a block whose one `DenseNodes` message is over the limit, nor
//! one where a single element fits only with the strings it uses. This cut
//! re-encodes its pieces instead, in two ways, each only where it is sure
//! the decoded content is unchanged:
//!
//! - **Dense groups are cut between nodes.** A `DenseNodes` message whose
//!   fields are all ones the reader knows, in the shapes it accepts (one
//!   data-bearing occurrence per column, every present column as long as
//!   the id column, `keys_vals` empty or one run per node), becomes one item
//!   per node. A piece takes a contiguous run of nodes as its own
//!   `DenseNodes`, in its own copy of the group: the first value of every
//!   delta-coded column is rebased to the absolute value the reader decodes
//!   there (with the reader's wrapping arithmetic), later deltas and the
//!   plain columns are copied, and every tag run ends in its terminator.
//!   Any other dense message stays one item.
//! - **Each piece gets its own string table**, holding entry 0 and the
//!   entries its elements use, in their original order, when every string
//!   reference in the block can be read: every element's fields are ones
//!   the reader knows, every index is within the table, and the table holds
//!   nothing but entries. Order is kept so every new index is at most the
//!   old one and every difference between two indexes at most the old
//!   difference, so no re-encoded varint is wider than the one it replaces.
//!   Otherwise every piece repeats the whole table and every index is kept
//!   as it is.
//!
//! A piece's size is bounded from above, not computed: the table entries it
//! keeps, its items' original bytes, and per group a field head at its
//! widest plus, for a group whose dense message is cut, every column's head
//! at its widest, a full-width absolute per delta column and one
//! terminator. Every term only grows as a piece takes an item at either
//! end, so the bound is monotone in both ends and the exact search applies.
//! What it cannot find: a partition that needs the bytes the bound
//! over-counts, one that exists only because index widths shrink, a cut
//! inside a dense message it cannot read, and a compact table where one
//! reference cannot be read. Those blocks go to the writer whole, and it
//! refuses them.

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use super::{Layout, Piece, partition};
use crate::error::Error;

/// Bytes of the widest length varint a block's field can need.
const LEN_WIDTH: usize = 5;
/// A one-byte field tag and the widest length.
const FIELD_HEAD: usize = 1 + LEN_WIDTH;
/// Bytes of the widest varint.
const ABS_WIDTH: usize = 10;

/// Group field numbers.
const NODE: u32 = 1;
const DENSE: u32 = 2;
const WAY: u32 = 3;
const RELATION: u32 = 4;

/// What the re-encoding cut does with one unit.
#[derive(Clone, Copy)]
enum Kind {
    /// A plain node, way or relation (`kind` is its group field), copied
    /// or re-encoded with remapped strings.
    Plain { kind: u32 },
    /// Copied as it is: a filler or a dense message this cut cannot read.
    Copy,
    /// Node `k` of the readable dense message `dense`.
    Node { dense: usize, k: usize },
}

struct Unit {
    group: usize,
    kind: Kind,
    /// The item's bytes in the block, tag included (not used for `Node`).
    field: (usize, usize),
    payload: (usize, usize),
    count: u64,
    /// Upper bound on its bytes in any piece.
    cost: usize,
    /// Its string references other than entry 0, a range of `Cut::slots`.
    slots: (usize, usize),
}

/// One column of a readable dense message: where its data lies in the
/// block and where each value's varint starts.
#[derive(Default)]
struct Column {
    data: (usize, usize),
    /// Offsets of every value's varint into the data, then its end.
    bounds: Vec<usize>,
}

impl Column {
    fn present(&self) -> bool {
        self.data.1 > self.data.0
    }

    fn value_bytes(&self, k: usize) -> usize {
        if self.present() {
            self.bounds[k + 1] - self.bounds[k]
        } else {
            0
        }
    }
}

/// One node's tag run in `keys_vals`.
struct Run {
    /// The run's bytes in the column data, terminator included if present.
    bytes: (usize, usize),
    /// Its keys and values, a range of `Dense::kv_values`.
    values: (usize, usize),
}

/// A dense message this cut can cut between nodes.
struct Dense {
    n: usize,
    id: Column,
    lat: Column,
    lon: Column,
    /// Whether a `DenseInfo` field was present at all.
    info: bool,
    /// `DenseInfo` columns 1-6: version, timestamp, changeset, uid,
    /// user_sid, visible.
    di: [Column; 6],
    /// `keys_vals`, empty when no node has tags.
    kv: Column,
    runs: Vec<Run>,
    kv_values: Vec<u64>,
    /// Absolute `user_sid` per node, when that column is present.
    user_sids: Vec<i32>,
}

/// `DenseInfo` columns that are delta coded, by index into `Dense::di`.
const DI_DELTA: [bool; 6] = [false, true, true, true, true, false];
/// Of those, the ones coded as `sint32` rather than `sint64`.
const DI_SINT32: [bool; 6] = [false, false, false, true, true, false];

pub(super) struct Cut<'b> {
    block: &'b [u8],
    layout: &'b Layout,
    units: Vec<Unit>,
    slots: Vec<u32>,
    dense: Vec<Dense>,
    /// Byte ranges of the string table's entries, tag included.
    entries: Vec<(usize, usize)>,
    /// Whether every piece gets its own table.
    compact: bool,
    /// Per group, the bytes it costs beyond its units.
    group_cost: Vec<usize>,
}

/// Plan a cut of `block` (taken apart as `layout`) into re-encoded pieces
/// of at most `limit` bytes: the cut and its cut points over its units, or
/// `None` when this cut finds no partition either. Nothing is assembled.
#[cold]
#[inline(never)]
pub(super) fn plan<'b>(
    block: &'b [u8],
    layout: &'b Layout,
    limit: usize,
) -> Option<(Cut<'b>, Vec<usize>)> {
    let c = Cut::new(block, layout);
    let counts: Vec<u64> = c.units.iter().map(|u| u.count).collect();
    let mut window = Window::new(&c);
    let cuts = partition(&counts, |i, j| window.size(i, j), limit)?;
    Some((c, cuts))
}

impl<'b> Cut<'b> {
    /// The planned piece holding units `[i, j)`, held to the bound the plan
    /// sized it by.
    pub(super) fn piece(&self, i: usize, j: usize, limit: usize) -> crate::Result<Piece> {
        let piece = self.assemble(i, j)?;
        if piece.bytes.len() > limit {
            return Err(Error::internal(format!(
                "block cut: a re-encoded piece is {} bytes, over its bound of {limit}",
                piece.bytes.len()
            )));
        }
        Ok(piece)
    }

    fn new(block: &'b [u8], layout: &'b Layout) -> Self {
        let (entries, mut compact) = match table_entries(block, layout.prefix) {
            Some(entries) => (entries, true),
            None => (Vec::new(), false),
        };
        let mut c = Cut {
            block,
            layout,
            units: Vec::with_capacity(layout.items.len()),
            slots: Vec::new(),
            dense: Vec::new(),
            entries,
            compact: false,
            group_cost: vec![FIELD_HEAD; layout.groups.len()],
        };
        let n_entries = c.entries.len();
        for (g, &(first, last)) in layout.groups.iter().enumerate() {
            for item in &layout.items[first..last] {
                let start = c.slots.len();
                let field_num = field_number(block, item.field);
                let alone = last - first == 1;
                if item.count == 0 {
                    compact &= empty_filler(block, item.field, item.payload, field_num);
                    c.push(g, Kind::Copy, item, start);
                } else if item.field_num == DENSE {
                    let payload = &block[item.payload.0..item.payload.1];
                    match read_dense(payload, item.payload.0).filter(|_| alone) {
                        Some(dense) => {
                            compact &= dense_strings_readable(&dense, n_entries);
                            c.push_dense(g, dense);
                        }
                        None => {
                            compact = false;
                            c.push(g, Kind::Copy, item, start);
                        }
                    }
                } else {
                    let payload = &block[item.payload.0..item.payload.1];
                    let mut scratch = Vec::new();
                    compact &= walk(
                        payload,
                        item.field_num,
                        n_entries,
                        None,
                        &mut c.slots,
                        &mut scratch,
                    );
                    c.push(
                        g,
                        Kind::Plain {
                            kind: item.field_num,
                        },
                        item,
                        start,
                    );
                }
            }
        }
        c.compact = compact;
        c
    }

    fn push(&mut self, group: usize, kind: Kind, item: &super::Item, slots_start: usize) {
        dedup_tail(&mut self.slots, slots_start);
        self.units.push(Unit {
            group,
            kind,
            field: item.field,
            payload: item.payload,
            count: item.count,
            cost: item.len(),
            slots: (slots_start, self.slots.len()),
        });
    }

    /// One unit per node of `dense`, and its group's fixed cost.
    fn push_dense(&mut self, group: usize, dense: Dense) {
        let d = self.dense.len();
        let mut fixed = FIELD_HEAD + 3 * (FIELD_HEAD + ABS_WIDTH);
        if dense.info {
            fixed += FIELD_HEAD;
            for (i, col) in dense.di.iter().enumerate() {
                if col.present() {
                    fixed += FIELD_HEAD + if DI_DELTA[i] { ABS_WIDTH } else { 0 };
                }
            }
        }
        if dense.kv.present() {
            fixed += FIELD_HEAD + 1;
        }
        self.group_cost[group] += fixed;
        for (k, slots) in node_slots(&dense).into_iter().enumerate() {
            let start = self.slots.len();
            self.slots.extend(slots);
            dedup_tail(&mut self.slots, start);
            let mut cost =
                dense.id.value_bytes(k) + dense.lat.value_bytes(k) + dense.lon.value_bytes(k);
            for col in &dense.di {
                cost += col.value_bytes(k);
            }
            if dense.kv.present() {
                let r = &dense.runs[k];
                cost += r.bytes.1 - r.bytes.0;
            }
            self.units.push(Unit {
                group,
                kind: Kind::Node { dense: d, k },
                field: (0, 0),
                payload: (0, 0),
                count: 1,
                cost,
                slots: (start, self.slots.len()),
            });
        }
        self.dense.push(dense);
    }

    fn entry_len(&self, slot: u32) -> usize {
        let (a, b) = self.entries[slot as usize];
        b - a
    }

    /// The piece holding units `[i, j)`.
    fn assemble(&self, i: usize, j: usize) -> crate::Result<Piece> {
        let block = self.block;
        let mut out = Vec::new();
        // Old slot to new, for the slots this piece keeps.
        let mut map: Vec<u32> = Vec::new();
        if self.compact {
            let mut kept: Vec<u32> = self.units[i..j]
                .iter()
                .flat_map(|u| self.slots[u.slots.0..u.slots.1].iter().copied())
                .collect();
            if !self.entries.is_empty() {
                kept.push(0);
            }
            kept.sort_unstable();
            kept.dedup();
            map = vec![0; self.entries.len()];
            let mut table = Vec::new();
            for (new, &old) in kept.iter().enumerate() {
                map[old as usize] = u32::try_from(new)
                    .map_err(|_| Error::internal("block cut: string slot overflow"))?;
                let (a, b) = self.entries[old as usize];
                table.extend_from_slice(&block[a..b]);
            }
            protohoggr::encode_bytes_field_always(&mut out, 1, &table);
        } else {
            out.extend_from_slice(&block[self.layout.prefix.0..self.layout.prefix.1]);
        }
        let map = self.compact.then_some(map.as_slice());
        let mut count = 0u64;
        let mut k = i;
        let mut body = Vec::new();
        let mut scratch = Vec::new();
        while k < j {
            let g = self.units[k].group;
            let mut end = k;
            while end < j && self.units[end].group == g {
                end += 1;
            }
            body.clear();
            if let Kind::Node { dense, k: first } = self.units[k].kind {
                let last = match self.units[end - 1].kind {
                    Kind::Node { k, .. } => k,
                    _ => return Err(Error::internal("block cut: a dense group mixing units")),
                };
                scratch.clear();
                self.emit_dense(&self.dense[dense], first, last + 1, map, &mut scratch)?;
                protohoggr::encode_bytes_field_always(&mut body, DENSE, &scratch);
            } else {
                for u in &self.units[k..end] {
                    match (u.kind, map) {
                        (Kind::Plain { kind }, Some(map)) => {
                            scratch.clear();
                            let payload = &block[u.payload.0..u.payload.1];
                            if !walk(
                                payload,
                                kind,
                                self.entries.len(),
                                Some(map),
                                &mut Vec::new(),
                                &mut scratch,
                            ) {
                                return Err(Error::internal(
                                    "block cut: an element readable once is not on its re-encoding",
                                ));
                            }
                            protohoggr::encode_bytes_field_always(&mut body, kind, &scratch);
                        }
                        _ => body.extend_from_slice(&block[u.field.0..u.field.1]),
                    }
                }
            }
            for u in &self.units[k..end] {
                count += u.count;
            }
            protohoggr::encode_bytes_field_always(&mut out, 2, &body);
            k = end;
        }
        out.extend_from_slice(&block[self.layout.suffix.0..self.layout.suffix.1]);
        Ok(Piece { bytes: out, count })
    }

    /// Nodes `[a, b)` of `dense` as one `DenseNodes` message.
    fn emit_dense(
        &self,
        dense: &Dense,
        a: usize,
        b: usize,
        map: Option<&[u32]>,
        out: &mut Vec<u8>,
    ) -> crate::Result<()> {
        let block = self.block;
        let mut col = Vec::new();
        rebased(block, &dense.id, a, b, false, &mut col)?;
        protohoggr::encode_bytes_field_always(out, 1, &col);
        if dense.info {
            let mut info = Vec::new();
            for (i, c) in dense.di.iter().enumerate() {
                if !c.present() {
                    continue;
                }
                col.clear();
                if i == 4 && map.is_some() {
                    remapped_sids(dense, a, b, map, &mut col)?;
                } else if DI_DELTA[i] {
                    rebased(block, c, a, b, DI_SINT32[i], &mut col)?;
                } else {
                    col.extend_from_slice(&block[c.data.0 + c.bounds[a]..c.data.0 + c.bounds[b]]);
                }
                protohoggr::encode_bytes_field_always(&mut info, field_u32(i + 1), &col);
            }
            protohoggr::encode_bytes_field_always(out, 5, &info);
        }
        for (field, c) in [(8, &dense.lat), (9, &dense.lon)] {
            col.clear();
            rebased(block, c, a, b, false, &mut col)?;
            protohoggr::encode_bytes_field_always(out, field, &col);
        }
        if dense.kv.present() {
            col.clear();
            for run in &dense.runs[a..b] {
                match map {
                    Some(map) => {
                        for &v in &dense.kv_values[run.values.0..run.values.1] {
                            protohoggr::encode_varint(&mut col, u64::from(map[slot(v)]));
                        }
                    }
                    None => {
                        let data = &block[dense.kv.data.0..dense.kv.data.1];
                        let raw = &data[run.bytes.0..run.bytes.1];
                        // The run's own bytes, its terminator excluded.
                        let mut cur = Cursor::new(raw);
                        let mut end = 0;
                        for _ in run.values.0..run.values.1 {
                            cur.read_varint().map_err(super::wire_err)?;
                            end = cur.position();
                        }
                        col.extend_from_slice(&raw[..end]);
                    }
                }
                col.push(0);
            }
            protohoggr::encode_bytes_field_always(out, 10, &col);
        }
        Ok(())
    }
}

fn field_u32(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// A slot index already checked against the table.
fn slot(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Drop repeats among `slots[start..]`.
fn dedup_tail(slots: &mut Vec<u32>, start: usize) {
    let mut tail = slots.split_off(start);
    tail.sort_unstable();
    tail.dedup();
    slots.extend(tail);
}

/// A delta-coded column's values `[a, b)`, the first rebased to the
/// absolute value the reader decodes there.
fn rebased(
    block: &[u8],
    c: &Column,
    a: usize,
    b: usize,
    sint32: bool,
    out: &mut Vec<u8>,
) -> crate::Result<()> {
    let data = &block[c.data.0..c.data.1];
    let mut cur = Cursor::new(&data[..c.bounds[a + 1]]);
    if sint32 {
        let mut abs: i32 = 0;
        while cur.remaining() > 0 {
            abs = abs.wrapping_add(cur.read_sint32().map_err(super::wire_err)?);
        }
        protohoggr::encode_varint(out, u64::from(zigzag32(abs)));
    } else {
        let mut abs: i64 = 0;
        while cur.remaining() > 0 {
            abs = abs.wrapping_add(cur.read_sint64().map_err(super::wire_err)?);
        }
        protohoggr::encode_varint(out, protohoggr::zigzag_encode_64(abs));
    }
    out.extend_from_slice(&data[c.bounds[a + 1]..c.bounds[b]]);
    Ok(())
}

fn zigzag32(v: i32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    let z = ((v << 1) ^ (v >> 31)) as u32;
    z
}

/// The `user_sid` column of nodes `[a, b)` with every sid remapped and
/// every delta taken again.
fn remapped_sids(
    dense: &Dense,
    a: usize,
    b: usize,
    map: Option<&[u32]>,
    out: &mut Vec<u8>,
) -> crate::Result<()> {
    let map = map.ok_or_else(|| Error::internal("block cut: remapping sids without a map"))?;
    let mut prev: i32 = 0;
    for &sid in &dense.user_sids[a..b] {
        let new = i32::try_from(map[slot(u64::try_from(sid).unwrap_or(u64::MAX))])
            .map_err(|_| Error::internal("block cut: a remapped sid overflows"))?;
        protohoggr::encode_varint(out, u64::from(zigzag32(new.wrapping_sub(prev))));
        prev = new;
    }
    Ok(())
}

/// The group field number of the item at `field`, 0 for an empty one.
fn field_number(block: &[u8], field: (usize, usize)) -> u32 {
    let mut cur = Cursor::new(&block[field.0..field.1]);
    match cur.read_tag() {
        Ok(Some((num, _))) => num,
        _ => 0,
    }
}

/// Whether a filler carries nothing a compact table could change: an empty
/// group body, or a `DenseNodes` whose every field is empty.
fn empty_filler(block: &[u8], field: (usize, usize), payload: (usize, usize), num: u32) -> bool {
    if field.0 == field.1 {
        return true;
    }
    if num != DENSE || payload.0 == field.0 {
        return false;
    }
    let mut cur = Cursor::new(&block[payload.0..payload.1]);
    loop {
        match cur.read_tag() {
            Ok(None) => return true,
            Ok(Some((_, WIRE_LEN))) => match cur.read_len_delimited() {
                Ok([]) => {}
                _ => return false,
            },
            _ => return false,
        }
    }
}

/// The string table's entries, or `None` when the table holds anything
/// else or is not one occurrence (the prefix the rewriters write).
fn table_entries(block: &[u8], prefix: (usize, usize)) -> Option<Vec<(usize, usize)>> {
    let bytes = &block[prefix.0..prefix.1];
    let mut cur = Cursor::new(bytes);
    if cur.read_tag().ok()? != Some((1, WIRE_LEN)) {
        return None;
    }
    let body = cur.read_len_delimited().ok()?;
    if cur.remaining() != 0 {
        return None;
    }
    let body_at = prefix.0 + cur.position() - body.len();
    let mut entries = Vec::new();
    let mut t = Cursor::new(body);
    loop {
        let start = t.position();
        match t.read_tag().ok()? {
            None => return Some(entries),
            Some((1, WIRE_LEN)) => {
                t.read_len_delimited().ok()?;
                entries.push((body_at + start, body_at + t.position()));
            }
            Some(_) => return None,
        }
    }
}

/// How the re-encoding treats one field of an element message.
enum Role {
    Copy,
    /// String indexes: packed, or one unpacked varint.
    Strings,
    Info,
}

fn element_role(kind: u32, field: u32, wire: u32) -> Option<Role> {
    let packed = wire == WIRE_LEN || wire == WIRE_VARINT;
    match (kind, field) {
        (_, 2 | 3) | (RELATION, 8) if packed => Some(Role::Strings),
        (_, 4) if wire == WIRE_LEN => Some(Role::Info),
        (NODE | WAY | RELATION, 1) if wire == WIRE_VARINT => Some(Role::Copy),
        (NODE, 8 | 9) if wire == WIRE_VARINT => Some(Role::Copy),
        (WAY, 8..=10) | (RELATION, 9 | 10) if packed => Some(Role::Copy),
        (WAY, 20) if wire == WIRE_LEN => Some(Role::Copy),
        _ => None,
    }
}

/// Walk an element message of group field `kind`. Without a `map`, push
/// every string index it holds into `slots` (entry 0 left out). With one,
/// write the message to `out` with every index remapped and every other
/// field copied, each in the form it had. `false` when a field is not one
/// the reader knows, in a shape it accepts, or an index is out of the
/// table.
fn walk(
    msg: &[u8],
    kind: u32,
    n_entries: usize,
    map: Option<&[u32]>,
    slots: &mut Vec<u32>,
    out: &mut Vec<u8>,
) -> bool {
    walk_fields(msg, n_entries, map, slots, out, &|field, wire| {
        element_role(kind, field, wire)
    })
}

fn info_role(field: u32, wire: u32) -> Option<Role> {
    match (field, wire) {
        (5, WIRE_VARINT) => Some(Role::Strings),
        (1..=4 | 6, WIRE_VARINT) => Some(Role::Copy),
        _ => None,
    }
}

fn walk_fields(
    msg: &[u8],
    n_entries: usize,
    map: Option<&[u32]>,
    slots: &mut Vec<u32>,
    out: &mut Vec<u8>,
    role: &dyn Fn(u32, u32) -> Option<Role>,
) -> bool {
    let mut cur = Cursor::new(msg);
    loop {
        let start = cur.position();
        let (field, wire) = match cur.read_tag() {
            Ok(Some(t)) => t,
            Ok(None) => return true,
            Err(_) => return false,
        };
        match role(field, wire) {
            None => return false,
            Some(Role::Copy) => {
                if cur.skip_field(wire).is_err() {
                    return false;
                }
                if map.is_some() {
                    out.extend_from_slice(&msg[start..cur.position()]);
                }
            }
            Some(Role::Strings) => {
                let values = if wire == WIRE_LEN {
                    match cur.read_len_delimited() {
                        Ok(v) => v,
                        Err(_) => return false,
                    }
                } else {
                    let at = cur.position();
                    if cur.read_varint().is_err() {
                        return false;
                    }
                    &msg[at..cur.position()]
                };
                let mut vals = Cursor::new(values);
                let mut remapped = Vec::new();
                while vals.remaining() > 0 {
                    let Ok(v) = vals.read_varint() else {
                        return false;
                    };
                    if v >= n_entries as u64 {
                        return false;
                    }
                    match map {
                        Some(map) => {
                            protohoggr::encode_varint(&mut remapped, u64::from(map[slot(v)]));
                        }
                        None if v != 0 => slots.push(u32::try_from(v).unwrap_or(u32::MAX)),
                        None => {}
                    }
                }
                if map.is_some() {
                    if wire == WIRE_LEN {
                        protohoggr::encode_bytes_field_always(out, field, &remapped);
                    } else {
                        protohoggr::encode_tag(out, field, WIRE_VARINT);
                        out.extend_from_slice(&remapped);
                    }
                }
            }
            Some(Role::Info) => {
                let Ok(info) = cur.read_len_delimited() else {
                    return false;
                };
                let mut inner = Vec::new();
                if !walk_fields(info, n_entries, map, slots, &mut inner, &info_role) {
                    return false;
                }
                if map.is_some() {
                    protohoggr::encode_bytes_field_always(out, field, &inner);
                }
            }
        }
    }
}

/// Take one column occurrence the way the reader does: packed or one
/// unpacked varint, empty occurrences ignored, a second data-bearing one
/// refused (`false`). `at` is the message's offset in the block.
fn take_column(cur: &mut Cursor<'_>, wire: u32, at: usize, slot: &mut (usize, usize)) -> bool {
    let (a, b) = if wire == WIRE_LEN {
        match cur.read_len_delimited() {
            Ok(v) => (cur.position() - v.len(), cur.position()),
            Err(_) => return false,
        }
    } else if wire == WIRE_VARINT {
        let a = cur.position();
        if cur.read_varint().is_err() {
            return false;
        }
        (a, cur.position())
    } else {
        return false;
    };
    if a == b {
        return true;
    }
    if slot.1 > slot.0 {
        return false;
    }
    *slot = (at + a, at + b);
    true
}

/// Varint boundaries of a column's data; `None` on a malformed varint.
fn bounds(block: &[u8], data: (usize, usize)) -> Option<Vec<usize>> {
    let mut cur = Cursor::new(&block[data.0..data.1]);
    let mut out = vec![0];
    while cur.remaining() > 0 {
        cur.read_varint().ok()?;
        out.push(cur.position());
    }
    Some(out)
}

/// A `keys_vals` column's runs, one per node, and its values; empty when
/// the column is. A last run may lack its terminator, as the reader allows;
/// `None` on a key without a value or a run count other than `n`.
fn tag_runs(data: &[u8], n: usize) -> Option<(Vec<Run>, Vec<u64>)> {
    let mut runs = Vec::new();
    let mut values = Vec::new();
    if data.is_empty() {
        return Some((runs, values));
    }
    let mut kc = Cursor::new(data);
    let mut run_start = 0;
    let mut values_start = 0;
    while kc.remaining() > 0 {
        let key = kc.read_varint().ok()?;
        if key == 0 {
            runs.push(Run {
                bytes: (run_start, kc.position()),
                values: (values_start, values.len()),
            });
            run_start = kc.position();
            values_start = values.len();
            continue;
        }
        let val = kc.read_varint().ok()?;
        values.push(key);
        values.push(val);
    }
    if run_start < data.len() {
        runs.push(Run {
            bytes: (run_start, data.len()),
            values: (values_start, values.len()),
        });
    }
    (runs.len() == n).then_some((runs, values))
}

/// The ranges, into `msg`, of a dense message's id, lat, lon and
/// `keys_vals` columns and its `DenseInfo` body, and whether it had a
/// `DenseInfo` at all; `None` on a field or shape the reader refuses or
/// this cut does not know.
fn dense_fields(msg: &[u8]) -> Option<([(usize, usize); 5], bool)> {
    let mut ranges = [(0usize, 0usize); 5];
    let mut info = false;
    let mut cur = Cursor::new(msg);
    while let Some((field, wire)) = cur.read_tag().ok()? {
        let slot = match field {
            1 => 0,
            8 => 1,
            9 => 2,
            10 => 3,
            5 if wire == WIRE_LEN => {
                let body = cur.read_len_delimited().ok()?;
                info = true;
                if body.is_empty() {
                    continue;
                }
                if ranges[4].1 > ranges[4].0 {
                    return None;
                }
                ranges[4] = (cur.position() - body.len(), cur.position());
                continue;
            }
            _ => return None,
        };
        if !take_column(&mut cur, wire, 0, &mut ranges[slot]) {
            return None;
        }
    }
    Some((ranges, info))
}

/// A dense message at block offset `at`, if this cut can cut it.
fn read_dense(msg: &[u8], at: usize) -> Option<Dense> {
    let (ranges, info) = dense_fields(msg)?;
    let block_range = |r: (usize, usize)| (at + r.0, at + r.1);
    let mut di_ranges = [(0usize, 0usize); 6];
    if ranges[4].1 > ranges[4].0 {
        let body = &msg[ranges[4].0..ranges[4].1];
        let mut ic = Cursor::new(body);
        while let Some((field, wire)) = ic.read_tag().ok()? {
            let i = usize::try_from(field)
                .ok()?
                .checked_sub(1)
                .filter(|&i| i < 6)?;
            if !take_column(&mut ic, wire, 0, &mut di_ranges[i]) {
                return None;
            }
        }
        for r in &mut di_ranges {
            if r.1 > r.0 {
                *r = (ranges[4].0 + r.0, ranges[4].0 + r.1);
            }
        }
    }
    // Column offsets so far are into `msg`; `bounds` reads the block, so
    // read them from `msg` with offset 0 and shift the data ranges after.
    let column = |r: (usize, usize)| -> Option<Column> {
        let bounds = bounds(msg, r)?;
        Some(Column {
            data: block_range(r),
            bounds,
        })
    };
    let id = column(ranges[0])?;
    let n = id.bounds.len() - 1;
    if n == 0 {
        return None;
    }
    let lat = column(ranges[1])?;
    let lon = column(ranges[2])?;
    if lat.bounds.len() - 1 != n || lon.bounds.len() - 1 != n {
        return None;
    }
    let mut di: [Column; 6] = Default::default();
    for (i, r) in di_ranges.iter().enumerate() {
        if r.1 > r.0 {
            let c = column(*r)?;
            if c.bounds.len() - 1 != n {
                return None;
            }
            di[i] = c;
        }
    }
    let kv_range = ranges[3];
    let (runs, kv_values) = tag_runs(&msg[kv_range.0..kv_range.1], n)?;
    let kv = Column {
        data: block_range(kv_range),
        bounds: Vec::new(),
    };
    let mut user_sids = Vec::new();
    if di[4].present() {
        let r = (di[4].data.0 - at, di[4].data.1 - at);
        let mut sc = Cursor::new(&msg[r.0..r.1]);
        let mut abs: i32 = 0;
        while sc.remaining() > 0 {
            abs = abs.wrapping_add(sc.read_sint32().ok()?);
            user_sids.push(abs);
        }
    }
    Some(Dense {
        n,
        id,
        lat,
        lon,
        info,
        di,
        kv,
        runs,
        kv_values,
        user_sids,
    })
}

/// Each node's string references (entry 0 left out).
fn node_slots(dense: &Dense) -> Vec<Vec<u32>> {
    let mut out = Vec::with_capacity(dense.n);
    for k in 0..dense.n {
        let mut s: Vec<u32> = Vec::new();
        if let Some(run) = dense.runs.get(k) {
            for &v in &dense.kv_values[run.values.0..run.values.1] {
                if v != 0 {
                    s.push(u32::try_from(v).unwrap_or(u32::MAX));
                }
            }
        }
        if let Some(&sid) = dense.user_sids.get(k)
            && sid > 0
        {
            s.push(u32::try_from(sid).unwrap_or(u32::MAX));
        }
        out.push(s);
    }
    out
}

/// Whether every string reference of `dense` is within the table.
fn dense_strings_readable(dense: &Dense, n_entries: usize) -> bool {
    let n = n_entries as u64;
    dense.kv_values.iter().all(|&v| v < n)
        && dense
            .user_sids
            .iter()
            .all(|&s| u64::try_from(s).is_ok_and(|s| s < n))
}

/// The size bound of the window `[lo, hi)` the partition search asks for,
/// kept incrementally: both ends only move forward.
struct Window<'c> {
    c: &'c Cut<'c>,
    /// Prefix sums of unit costs.
    costs: Vec<usize>,
    /// Prefix sums of group costs.
    groups: Vec<usize>,
    /// Bytes outside the groups that do not depend on the window.
    fixed: usize,
    refs: Vec<u32>,
    table: usize,
    lo: usize,
    hi: usize,
}

impl<'c> Window<'c> {
    fn new(c: &'c Cut<'c>) -> Self {
        let mut costs = Vec::with_capacity(c.units.len() + 1);
        costs.push(0);
        for u in &c.units {
            costs.push(costs[costs.len() - 1] + u.cost);
        }
        let mut groups = Vec::with_capacity(c.group_cost.len() + 1);
        groups.push(0);
        for &g in &c.group_cost {
            groups.push(groups[groups.len() - 1] + g);
        }
        let suffix = c.layout.suffix.1 - c.layout.suffix.0;
        let fixed = if c.compact {
            suffix
                + FIELD_HEAD
                + if c.entries.is_empty() {
                    0
                } else {
                    c.entry_len(0)
                }
        } else {
            suffix + (c.layout.prefix.1 - c.layout.prefix.0)
        };
        Self {
            c,
            costs,
            groups,
            fixed,
            refs: vec![0; c.entries.len()],
            table: 0,
            lo: 0,
            hi: 0,
        }
    }

    fn slots(&self, u: usize) -> &'c [u32] {
        let c = self.c;
        let unit = &c.units[u];
        &c.slots[unit.slots.0..unit.slots.1]
    }

    fn size(&mut self, i: usize, j: usize) -> usize {
        if self.c.compact {
            while self.hi < j {
                for &s in self.slots(self.hi) {
                    let r = &mut self.refs[s as usize];
                    if *r == 0 {
                        self.table += self.c.entry_len(s);
                    }
                    *r += 1;
                }
                self.hi += 1;
            }
            while self.lo < i {
                for &s in self.slots(self.lo) {
                    let r = &mut self.refs[s as usize];
                    *r -= 1;
                    if *r == 0 {
                        self.table -= self.c.entry_len(s);
                    }
                }
                self.lo += 1;
            }
        }
        let gi = self.c.units[i].group;
        let gj = self.c.units[j - 1].group;
        self.fixed
            + self.table
            + (self.costs[j] - self.costs[i])
            + (self.groups[gj + 1] - self.groups[gi])
    }
}
