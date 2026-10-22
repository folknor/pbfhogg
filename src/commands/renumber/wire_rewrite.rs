//! Wire-format rewriters for DenseNodes, Ways, and Relations.
//!
//! These are the renumber-specific fast path: renumber only changes IDs,
//! so we avoid the full decode -> BlockBuilder -> re-encode cycle.
//! Per-node / per-way / per-relation cost drops from ~113 ns (HashMap
//! lookups, delta arrays, metadata) to ~10-15 ns (varint decode of old
//! ID + varint encode of new delta) plus verbatim byte-range copies for
//! all other fields.
//!
//! # Every integer here comes off the wire
//!
//! Delta-encoded IDs are summed, positions are offset, counts are
//! accumulated - all from bytes the input controls. A crafted delta that
//! overflowed an `i64` used to panic in debug builds and wrap in release,
//! landing anywhere. The lint below makes every arithmetic operator in this
//! file a compile error, so each one is a `checked_*` call mapped to an
//! input error, or a method whose saturation is the intended meaning.
//!
//! # Ordering is part of the contract
//!
//! Renumber gives an element its new ID by *position* and resolves every
//! reference by *rank* among the input IDs; the two agree only when input
//! IDs strictly increase. Each rewriter refuses an ID that does not exceed
//! the previous one in the blob, and returns the first and last input ID it
//! saw so the stage driver can check the boundary between blobs
//! (`pool::StrictOrder`). A header's `Sort.Type_then_ID` claim is not
//! evidence: nothing verifies it.
#![deny(clippy::arithmetic_side_effects)]

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use crate::idset::IdSet;

/// Error text for a packed field this rewriter cannot splice.
///
/// Unlike `src/scan/*` and `src/blob_meta/scan.rs`, these rewriters cannot
/// route through `src/read/wire.rs`. `wire.rs` yields a packed field's *value*
/// as a borrowed slice; a byte-level splice needs the field's *byte range*
/// inside the enclosing message - the tag start and the value end - so that
/// every other field can be copied verbatim without a parse / re-encode. That
/// is information the shared parser deliberately does not expose. The
/// duplicate-occurrence half of the policy is therefore re-decided here rather
/// than inherited - see [`repeated_field_error`], which refuses a second
/// data-bearing occurrence of the same packed field for reasons a splice makes
/// sharper than the read path's.
///
/// So the policy is the same as `wire.rs` in its *observable* effect - never
/// silently mis-handle a legal encoding - but reached by refusing rather than
/// accepting. Concretely, an unpacked `DenseNodes.id`, `Way.refs`,
/// `Relation.memids` or `Relation.types` is spec-legal and rare (no known
/// writer emits it), and the old code's handling ranged from a misleading
/// error to genuine silent corruption: unpacked `memids` left `memids_range`
/// as `None`, so the relation was treated as memberless, the id was patched,
/// and the unpacked member IDs were copied through **un-renumbered** into
/// output that claims to be renumbered.
fn unpacked_field_error(message: &str, field: &str) -> String {
    format!(
        "renumber: {message}.{field} is written in the unpacked wire form \
         (individually tagged varints). That encoding is spec-legal but the \
         renumber wire-splice rewriter can only patch the length-delimited \
         packed form. Re-encode the input first, e.g. `pbfhogg cat --clean`."
    )
}

/// Error text for a packed field that occurs more than once carrying data.
///
/// Protobuf gives a repeated packed field *concatenation* semantics across
/// occurrences: two length-delimited segments of `Way.refs` are one ref array,
/// not two candidate arrays. `src/read/wire.rs::take_packed` refuses that shape
/// because the read path is built on a single borrowed slice; these rewriters
/// have to refuse it for a stronger reason still. A splice replaces one
/// contiguous byte range and copies everything else verbatim, so with two
/// segments the earlier one is copied through **unrenumbered** while only the
/// last is patched - partially renumbered output, silently, from a file that
/// is entirely legal protobuf. For `Relation.types` the damage is worse than
/// partial: `types` drives the per-member node/way/relation dispatch, so
/// keeping only the last segment makes `memids` and `types` different lengths
/// and the count cross-check fires with a misleading message, or (equal
/// lengths, different values) every member is looked up in the wrong ID space.
///
/// Empty occurrences are ignored in either position, matching `take_packed`:
/// a zero-length segment carries no elements and cannot participate in a
/// concatenation.
fn repeated_field_error(message: &str, field: &str) -> String {
    format!(
        "renumber: {message}.{field} appears twice with data; packed field split \
         across segments. Protobuf concatenates such segments, which a byte-level \
         splice cannot express as one contiguous range - the earlier segment would \
         be copied through unrenumbered. Re-encode the input first, e.g. \
         `pbfhogg cat --clean`."
    )
}

/// Error text for a message that repeats a field the rewriter must read as
/// one value. An element's second `id` would be copied through verbatim
/// beside the patched one (protobuf takes the last, so which ID the output
/// "has" depends on the reader), and a second `DenseNodes` or `StringTable`
/// would be merged by protobuf but read here as just one of the two.
fn repeated_message_field_error(message: &str, field: &str) -> String {
    format!(
        "renumber: {message} carries {field} more than once. Protobuf merges or \
         overrides repeated occurrences, which the wire-splice rewriter does not \
         reproduce. Re-encode the input first, e.g. `pbfhogg cat --clean`."
    )
}

/// Error text for a PrimitiveGroup that holds an element kind other than
/// the one its blob is being rewritten as. A group holds one kind; the
/// rewriters used to drop the stranger (node and relation rewriters) or copy
/// it through un-renumbered (way rewriter), both silently.
fn foreign_kind_error(rewriting: &str, field: u32) -> String {
    let found = match field {
        1 => "plain Node",
        2 => "DenseNodes",
        3 => "Way",
        _ => "Relation",
    };
    format!(
        "renumber: a {rewriting} blob's PrimitiveGroup also carries {found} \
         elements. A group must hold one element kind; renumber cannot rewrite \
         a mixed group without dropping or mis-numbering part of it."
    )
}

/// Error text for input IDs that are not strictly increasing.
pub(super) fn order_error(kind: &str, prev: i64, id: i64) -> String {
    let what = if id == prev {
        "appears twice"
    } else {
        "comes after a larger id"
    };
    format!(
        "renumber requires {kind} ids to strictly increase in file order, but \
         {kind} {id} {what} ({kind} {prev} precedes it). New ids are assigned by \
         position and references resolved by rank, which agree only on sorted, \
         duplicate-free input; a Sort.Type_then_ID header claim is not checked. \
         Sort the input (pbfhogg sort) and remove duplicate ids before renumbering."
    )
}

fn delta_overflow_error(field: &str) -> String {
    format!(
        "renumber: {field} delta sum overflows a 64-bit id; the input's delta \
         encoding is corrupt."
    )
}

fn negative_id_error(kind: &str, id: i64) -> String {
    format!(
        "renumber requires non-negative input ids. \
         Input contains {kind} id {id}. \
         Negative ids are JOSM editor-local staging identifiers \
         that should be resolved before processing."
    )
}

fn wire_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// `start + rank` as an output ID.
fn offset_id(start: i64, rank: u64) -> Result<i64, String> {
    i64::try_from(rank)
        .ok()
        .and_then(|r| start.checked_add(r))
        .ok_or_else(|| format!("renumber: new id {start} + {rank} overflows a 64-bit id"))
}

fn inc(n: u64) -> Result<u64, String> {
    n.checked_add(1)
        .ok_or_else(|| "renumber: element count overflow".to_string())
}

/// One element kind's old-to-new ID mapping, for resolving references.
///
/// An ID in the set becomes `start + rank`. An orphan - a reference to an
/// object absent from the input - becomes `orphan_base + old_id`, where
/// `orphan_base = start + count` is the first ID past the renumbered range.
/// Passing orphans through unchanged let an old ID equal a new ID given to a
/// different object, so a reference silently pointed at the wrong element.
/// The shift keeps every orphan outside the new range, keeps two references
/// to one orphan equal, and keeps the old ID recoverable by subtraction
/// (DEVIATIONS.md, "renumber: orphan-reference handling").
///
/// The base is held as an `i128`: a live range may end exactly at
/// `i64::MAX`, which leaves no representable base, and that input is only
/// an error once an orphan actually needs one.
#[derive(Clone, Copy)]
pub(super) struct IdSpace<'a> {
    kind: &'static str,
    set: &'a IdSet,
    start: i64,
    orphan_base: i128,
}

impl<'a> IdSpace<'a> {
    /// `set` must be complete and carry its rank index (`total_count` and
    /// `rank_if_set` read it), so build this only once every ID of `kind`
    /// has been recorded.
    pub(super) fn new(kind: &'static str, set: &'a IdSet, start: i64) -> Self {
        let orphan_base = i128::from(start).saturating_add(i128::from(set.total_count()));
        Self {
            kind,
            set,
            start,
            orphan_base,
        }
    }

    /// The new ID of an element that must be in the set (a relation's own
    /// ID in R2d). `None` when it is not.
    #[inline]
    fn live(&self, old_id: i64) -> Result<Option<i64>, String> {
        self.set
            .rank_if_set(old_id)
            .map(|r| offset_id(self.start, r))
            .transpose()
    }

    /// `(new_id, is_orphan)` for a reference, with a single chunk lookup.
    #[inline]
    fn resolve(&self, old_id: i64) -> Result<(i64, bool), String> {
        if let Some(new_id) = self.live(old_id)? {
            return Ok((new_id, false));
        }
        let shifted = self.orphan_base.saturating_add(i128::from(old_id));
        i64::try_from(shifted).map(|id| (id, true)).map_err(|_| {
            format!(
                "renumber: orphan {kind} ref {old_id} (the {kind} is not in the input) \
                 would become {shifted}, the first id past the renumbered {kind}s \
                 ({base}) plus its old id, which overflows a 64-bit id",
                kind = self.kind,
                base = self.orphan_base,
            )
        })
    }
}

/// Count varints in `data`, erroring on a truncated trailing varint or a
/// malformed mid-stream byte. Counting continuation-bit-clear bytes is
/// faster but silently accepts truncation (no terminator means the last
/// byte is missed; extra `0x80` bytes after the last terminator are
/// silently ignored). A full walk catches both classes at the boundary.
fn count_varints_strict(data: &[u8]) -> Result<usize, String> {
    let mut cursor = Cursor::new(data);
    let mut count = 0_usize;
    while cursor.remaining() > 0 {
        cursor.read_varint().map_err(wire_err)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| "renumber: varint count overflow".to_string())?;
    }
    Ok(count)
}

/// First and last input ID seen, refusing any ID that does not exceed the
/// one before it.
#[derive(Default)]
struct Span {
    first: Option<i64>,
    last: Option<i64>,
}

impl Span {
    fn push(&mut self, kind: &str, id: i64) -> Result<(), String> {
        if let Some(prev) = self.last
            && id <= prev
        {
            return Err(order_error(kind, prev, id));
        }
        if self.first.is_none() {
            self.first = Some(id);
        }
        self.last = Some(id);
        Ok(())
    }

    fn get(&self) -> Option<(i64, i64)> {
        Some((self.first?, self.last?))
    }
}

/// Reusable buffers for one worker's rewrites.
#[derive(Default)]
pub(super) struct Scratch {
    scalars: Vec<u8>,
    other_fields: Vec<u8>,
    packed: Vec<u8>,
    element: Vec<u8>,
    group: Vec<u8>,
}

/// What a rewrite saw in one block.
pub(super) struct Outcome {
    /// Elements rewritten.
    pub(super) count: u64,
    /// References whose target is not in the input, shifted past the
    /// renumbered range (see [`IdSpace`]).
    pub(super) orphans: u64,
    /// First and last input ID, `None` when the block held no elements.
    pub(super) span: Option<(i64, i64)>,
    /// Smallest and largest new ID assigned (relations only; node and way
    /// ranges follow from the blob base).
    pub(super) new_range: Option<(i64, i64)>,
}

/// Walk a PrimitiveBlock's top level once: return its StringTable and copy
/// the top-level fields to keep into `scalars` (granularity / offsets for a
/// node block, every non-group field otherwise). Groups are visited by a
/// second walk in [`for_each_group`], so no offsets into the block are
/// stored between the two.
fn split_block<'a>(
    block: &'a [u8],
    keep_all: bool,
    scalars: &mut Vec<u8>,
    who: &str,
) -> Result<&'a [u8], String> {
    scalars.clear();
    let mut stringtable: Option<&[u8]> = None;
    let mut cursor = Cursor::new(block);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(wire_err)?;
                if stringtable.replace(data).is_some() {
                    return Err(repeated_message_field_error(
                        "PrimitiveBlock",
                        "StringTable",
                    ));
                }
            }
            (2, WIRE_LEN) => cursor.skip_field(wire_type).map_err(wire_err)?,
            (17..=20, WIRE_VARINT) => copy_field(&mut cursor, field, wire_type, scalars)?,
            _ if keep_all => copy_field(&mut cursor, field, wire_type, scalars)?,
            _ => cursor.skip_field(wire_type).map_err(wire_err)?,
        }
    }
    stringtable.ok_or_else(|| format!("{who}: no StringTable in PrimitiveBlock"))
}

fn copy_field(
    cursor: &mut Cursor<'_>,
    field: u32,
    wire_type: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let raw = cursor.read_raw_field(wire_type).map_err(wire_err)?;
    protohoggr::encode_tag(out, field, wire_type);
    out.extend_from_slice(raw);
    Ok(())
}

/// Call `f` on every PrimitiveGroup of `block`, in order. Returns how many.
fn for_each_group<'a>(
    block: &'a [u8],
    mut f: impl FnMut(&'a [u8]) -> Result<(), String>,
) -> Result<u64, String> {
    let mut groups: u64 = 0;
    let mut cursor = Cursor::new(block);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? {
        if field == 2 && wire_type == WIRE_LEN {
            f(cursor.read_len_delimited().map_err(wire_err)?)?;
            groups = inc(groups)?;
        } else {
            cursor.skip_field(wire_type).map_err(wire_err)?;
        }
    }
    Ok(groups)
}

// ---------------------------------------------------------------------------
// DenseNodes wire-format rewriter for pass 1
// ---------------------------------------------------------------------------

/// The one DenseNodes message of a node group. Refuses a second one (protobuf
/// would merge the two, concatenating their packed arrays, where taking
/// either alone silently drops nodes) and any other element kind.
fn single_dense(group: &[u8]) -> Result<&[u8], String> {
    let mut dense: Option<&[u8]> = None;
    let mut cursor = Cursor::new(group);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited().map_err(wire_err)?;
                if dense.replace(data).is_some() {
                    return Err(repeated_message_field_error("PrimitiveGroup", "DenseNodes"));
                }
            }
            (1 | 3 | 4, _) => return Err(foreign_kind_error("node", field)),
            _ => cursor.skip_field(wire_type).map_err(wire_err)?,
        }
    }
    dense.ok_or_else(|| "reframe: no DenseNodes in PrimitiveGroup".to_string())
}

/// Reframe a decompressed PrimitiveBlock by replacing only the DenseNodes
/// ID deltas while copying everything else (string table, lat/lon, tags,
/// metadata) verbatim at the byte level.
///
/// Records each old node ID in `id_set` through the atomic setter. An ID
/// whose chunk `id_set` does not cover - the pre-allocation is bounded by the
/// input's size, not by its indexdata's claims - is appended to `overflow`
/// instead, for the caller to insert once the parallel pass has ended.
pub(super) fn reframe_dense_with_new_ids(
    decompressed: &[u8],
    base_new_id: i64,
    id_set: &IdSet,
    overflow: &mut Vec<i64>,
    output: &mut Vec<u8>,
    s: &mut Scratch,
) -> Result<Outcome, String> {
    let stringtable = split_block(decompressed, false, &mut s.scalars, "reframe")?;
    output.clear();
    protohoggr::encode_bytes_field_always(output, 1, stringtable);

    let mut assigned: u64 = 0;
    let mut span = Span::default();
    let groups = for_each_group(decompressed, |group| {
        let dense_bytes = single_dense(group)?;

        // `id_field` holds only a data-bearing occurrence; `saw_id_field`
        // records that field 1 was present at all, so a DenseNodes carrying
        // one empty packed `id` still means "zero nodes" rather than "no id
        // field".
        let mut id_field: Option<&[u8]> = None;
        let mut saw_id_field = false;
        s.other_fields.clear();
        let mut dn_cursor = Cursor::new(dense_bytes);
        while let Some((field, wire_type)) = dn_cursor.read_tag().map_err(wire_err)? {
            if field == 1 && wire_type == WIRE_VARINT {
                return Err(unpacked_field_error("DenseNodes", "id"));
            }
            if field == 1 && wire_type == WIRE_LEN {
                let seg = dn_cursor.read_len_delimited().map_err(wire_err)?;
                saw_id_field = true;
                if !seg.is_empty() {
                    if id_field.is_some() {
                        return Err(repeated_field_error("DenseNodes", "id"));
                    }
                    id_field = Some(seg);
                }
            } else {
                copy_field(&mut dn_cursor, field, wire_type, &mut s.other_fields)?;
            }
        }
        if !saw_id_field {
            return Err("reframe: no packed ID field in DenseNodes".into());
        }

        let mut old_id: i64 = 0;
        let mut id_cursor = Cursor::new(id_field.unwrap_or_default());
        let mut group_count: u64 = 0;
        while id_cursor.remaining() > 0 {
            let delta = id_cursor.read_sint64().map_err(wire_err)?;
            old_id = old_id
                .checked_add(delta)
                .ok_or_else(|| delta_overflow_error("DenseNodes.id"))?;
            // Unconditional: indexdata `min_id >= 0` is a claim, not a check.
            // See DEVIATIONS.md > "Negative input IDs rejected project-wide".
            if old_id < 0 {
                return Err(negative_id_error("node", old_id));
            }
            span.push("node", old_id)?;
            if !id_set.set_atomic_checked(old_id) {
                overflow.push(old_id);
            }
            group_count = inc(group_count)?;
        }

        // New IDs are consecutive from this group's base: the first delta is
        // the absolute ID, every later one is +1 (zigzag 2). A group with no
        // nodes gets no ID at all - writing its base anyway would fabricate a
        // node that duplicates the next group's first.
        s.packed.clear();
        if group_count > 0 {
            let first_new = offset_id(base_new_id, assigned)?;
            protohoggr::encode_varint(&mut s.packed, protohoggr::zigzag_encode_64(first_new));
            let n = usize::try_from(group_count).map_err(|_| "group node count > usize")?;
            s.packed
                .extend(std::iter::repeat_n(0x02u8, n.saturating_sub(1)));
        }
        assigned = assigned
            .checked_add(group_count)
            .ok_or_else(|| "renumber: node count overflow".to_string())?;

        s.element.clear();
        // Always written, even empty: a DenseNodes with no `id` field is
        // refused on input ("no packed ID field"), so an empty group that
        // still carries other fields must keep its empty `id`.
        protohoggr::encode_bytes_field_always(&mut s.element, 1, &s.packed);
        s.element.extend_from_slice(&s.other_fields);
        s.group.clear();
        protohoggr::encode_bytes_field(&mut s.group, 2, &s.element);
        protohoggr::encode_bytes_field(output, 2, &s.group);
        Ok(())
    })?;
    if groups == 0 {
        return Err("reframe: no PrimitiveGroup in PrimitiveBlock".into());
    }
    output.extend_from_slice(&s.scalars);

    Ok(Outcome {
        count: assigned,
        orphans: 0,
        span: span.get(),
        new_range: None,
    })
}

// ---------------------------------------------------------------------------
// Way rewriter for stage 2d
// ---------------------------------------------------------------------------

/// Reframe a decompressed way-blob PrimitiveBlock by replacing way IDs
/// and node refs while copying everything else verbatim.
///
/// For each way: decode old way id, assign new sequential way id,
/// resolve each ref's new node id through `nodes`, delta-encode the new
/// refs, and copy keys/vals/info raw bytes verbatim.
pub(super) fn reframe_ways_with_new_ids(
    decompressed: &[u8],
    base_new_way_id: i64,
    nodes: &IdSpace<'_>,
    way_id_set: &mut IdSet,
    output: &mut Vec<u8>,
    s: &mut Scratch,
) -> Result<Outcome, String> {
    let stringtable = split_block(decompressed, true, &mut s.scalars, "reframe_ways")?;
    output.clear();
    protohoggr::encode_bytes_field_always(output, 1, stringtable);

    let mut total: u64 = 0;
    let mut orphans: u64 = 0;
    let mut span = Span::default();
    for_each_group(decompressed, |group| {
        s.group.clear();
        let mut gr_cursor = Cursor::new(group);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(wire_err)? {
            match (field, wire_type) {
                (3, WIRE_LEN) => {
                    let way_bytes = gr_cursor.read_len_delimited().map_err(wire_err)?;
                    let new_way_id = offset_id(base_new_way_id, total)?;
                    let way_orphans = reframe_way(
                        way_bytes,
                        new_way_id,
                        nodes,
                        way_id_set,
                        &mut span,
                        &mut s.packed,
                        &mut s.element,
                    )?;
                    orphans = orphans
                        .checked_add(way_orphans)
                        .ok_or_else(|| "renumber: orphan count overflow".to_string())?;
                    protohoggr::encode_bytes_field(&mut s.group, 3, &s.element);
                    total = inc(total)?;
                }
                (1 | 2 | 4, _) => return Err(foreign_kind_error("way", field)),
                // Any other group field is copied verbatim.
                _ => copy_field(&mut gr_cursor, field, wire_type, &mut s.group)?,
            }
        }
        protohoggr::encode_bytes_field(output, 2, &s.group);
        Ok(())
    })?;
    output.extend_from_slice(&s.scalars);

    Ok(Outcome {
        count: total,
        orphans,
        span: span.get(),
        new_range: None,
    })
}

/// Rewrite one Way into `out` with its id and refs replaced. Returns the
/// number of orphan refs.
#[allow(clippy::too_many_arguments)]
fn reframe_way(
    way_bytes: &[u8],
    new_way_id: i64,
    nodes: &IdSpace<'_>,
    way_id_set: &mut IdSet,
    span: &mut Span,
    refs_out: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<u64, String> {
    // (tag_start, value_end) of the fields being replaced. The tag start is
    // the cursor position before the tag is read, so a multi-byte tag is
    // spliced correctly too.
    let mut id: Option<(i64, (usize, usize))> = None;
    let mut refs_range: Option<(usize, usize)> = None;
    let mut old_refs_data: &[u8] = &[];

    let mut cursor = Cursor::new(way_bytes);
    loop {
        let tag_start = cursor.position();
        let Some((wf, wt)) = cursor.read_tag().map_err(wire_err)? else {
            break;
        };
        match (wf, wt) {
            (1, WIRE_VARINT) => {
                let v = cursor.read_varint_i64().map_err(wire_err)?;
                if id.replace((v, (tag_start, cursor.position()))).is_some() {
                    return Err(repeated_message_field_error("Way", "id"));
                }
            }
            (8, WIRE_VARINT) => return Err(unpacked_field_error("Way", "refs")),
            (8, WIRE_LEN) => {
                let seg = cursor.read_len_delimited().map_err(wire_err)?;
                if !seg.is_empty() && !old_refs_data.is_empty() {
                    return Err(repeated_field_error("Way", "refs"));
                }
                // An empty first occurrence is recorded so a zero-ref way
                // still splices; a later data-bearing one takes over.
                if refs_range.is_none() || !seg.is_empty() {
                    old_refs_data = seg;
                    refs_range = Some((tag_start, cursor.position()));
                }
            }
            _ => {
                cursor.read_raw_field(wt).map_err(wire_err)?;
            }
        }
    }

    let (old_way_id, id_r) = id.ok_or("reframe_ways: no id field")?;
    // Unconditional: a negative way id slipping past stale indexdata would
    // be silently dropped by `way_id_set.set` (which ignores `id < 0`),
    // producing phantom orphan refs in relations.
    if old_way_id < 0 {
        return Err(negative_id_error("way", old_way_id));
    }
    span.push("way", old_way_id)?;
    way_id_set.set(old_way_id);

    // Decode old ref deltas, resolve via rank, delta-encode new refs.
    refs_out.clear();
    let mut orphans: u64 = 0;
    let mut prev_old_ref: i64 = 0;
    let mut prev_new_ref: i64 = 0;
    let mut refs_cursor = Cursor::new(old_refs_data);
    while !refs_cursor.is_empty() {
        let delta = refs_cursor.read_sint64().map_err(wire_err)?;
        prev_old_ref = prev_old_ref
            .checked_add(delta)
            .ok_or_else(|| delta_overflow_error("Way.refs"))?;
        let old_node_id = prev_old_ref;
        if old_node_id < 0 {
            return Err(format!(
                "renumber requires non-negative input ids. \
                 Way references negative node id {old_node_id}. \
                 Negative ids are JOSM editor-local staging \
                 identifiers that should be resolved before \
                 processing."
            ));
        }
        let (new_ref, is_orphan) = nodes
            .resolve(old_node_id)
            .map_err(|e| format!("{e} (in way {old_way_id})"))?;
        if is_orphan {
            orphans = inc(orphans)?;
        }
        let new_delta = new_ref
            .checked_sub(prev_new_ref)
            .ok_or_else(|| delta_overflow_error("Way.refs (output)"))?;
        protohoggr::encode_varint(refs_out, protohoggr::zigzag_encode_64(new_delta));
        prev_new_ref = new_ref;
    }

    let refs_r = refs_range.ok_or("reframe_ways: no refs field")?;
    splice_two(
        way_bytes,
        id_r,
        |o| protohoggr::encode_int64_field(o, 1, new_way_id),
        refs_r,
        |o| protohoggr::encode_bytes_field(o, 8, refs_out),
        out,
    );
    Ok(orphans)
}

/// Emit `bytes` into `out` with the two non-overlapping ranges `a` and `b`
/// replaced by what `emit_a` / `emit_b` write, in whichever order the ranges
/// occur.
fn splice_two(
    bytes: &[u8],
    a: (usize, usize),
    emit_a: impl FnOnce(&mut Vec<u8>),
    b: (usize, usize),
    emit_b: impl FnOnce(&mut Vec<u8>),
    out: &mut Vec<u8>,
) {
    out.clear();
    if a.0 < b.0 {
        out.extend_from_slice(&bytes[..a.0]);
        emit_a(out);
        out.extend_from_slice(&bytes[a.1..b.0]);
        emit_b(out);
        out.extend_from_slice(&bytes[b.1..]);
    } else {
        out.extend_from_slice(&bytes[..b.0]);
        emit_b(out);
        out.extend_from_slice(&bytes[b.1..a.0]);
        emit_a(out);
        out.extend_from_slice(&bytes[a.1..]);
    }
}

// ---------------------------------------------------------------------------
// Wire-format relation rewriter
// ---------------------------------------------------------------------------

/// The id of one Relation message, read under the same rule R2d's rewriter
/// applies: exactly one varint field 1. R1 collects the relation ID set
/// through this, so the ID it records is always the one R2d later looks up
/// (R1 used to stop at the first id field while R2d kept the last).
pub(super) fn relation_id(rel_bytes: &[u8]) -> Result<i64, String> {
    let mut id: Option<i64> = None;
    let mut cursor = Cursor::new(rel_bytes);
    while let Some((field, wire_type)) = cursor.read_tag().map_err(wire_err)? {
        if field == 1 && wire_type == WIRE_VARINT {
            let v = cursor.read_varint_i64().map_err(wire_err)?;
            if id.replace(v).is_some() {
                return Err(repeated_message_field_error("Relation", "id"));
            }
        } else {
            cursor.skip_field(wire_type).map_err(wire_err)?;
        }
    }
    id.ok_or_else(|| "reframe_relations: relation without an id field".to_string())
}

/// The three ID spaces a relation's members resolve against.
pub(super) struct RelationIdSpaces<'a> {
    pub(super) relations: IdSpace<'a>,
    pub(super) nodes: IdSpace<'a>,
    pub(super) ways: IdSpace<'a>,
}

/// Wire-format splice rewriter for relations. Patches field 1 (id) and
/// field 9 (memids) in each Relation submessage; copies all other fields
/// (keys, vals, info, roles_sid, types) verbatim as raw bytes.
///
/// The memids field (packed sint64, delta-encoded) interleaves node, way,
/// and relation member IDs in one stream. Field 10 (types, packed int32)
/// tells us which lookup to use for each member:
///   0 = node, 1 = way, 2 = relation, other = unknown (old ID preserved).
///
/// One `prev_new_id` accumulator tracks across ALL member types - the
/// delta encoding is over the interleaved stream, not per-type.
pub(super) fn reframe_relations_with_new_ids(
    decompressed: &[u8],
    ids: &RelationIdSpaces<'_>,
    output: &mut Vec<u8>,
    s: &mut Scratch,
) -> Result<Outcome, String> {
    let stringtable = split_block(decompressed, true, &mut s.scalars, "reframe_relations")?;
    output.clear();
    protohoggr::encode_bytes_field_always(output, 1, stringtable);

    let mut total: u64 = 0;
    let mut orphans: u64 = 0;
    let mut span = Span::default();
    let mut new_range: Option<(i64, i64)> = None;
    for_each_group(decompressed, |group| {
        s.group.clear();
        let mut gr_cursor = Cursor::new(group);
        while let Some((field, wire_type)) = gr_cursor.read_tag().map_err(wire_err)? {
            match (field, wire_type) {
                (4, WIRE_LEN) => {
                    let rel_bytes = gr_cursor.read_len_delimited().map_err(wire_err)?;
                    let (new_id, rel_orphans) =
                        reframe_relation(rel_bytes, ids, &mut span, &mut s.packed, &mut s.element)?;
                    orphans = orphans
                        .checked_add(rel_orphans)
                        .ok_or_else(|| "renumber: orphan count overflow".to_string())?;
                    new_range = Some(match new_range {
                        None => (new_id, new_id),
                        Some((lo, hi)) => (lo.min(new_id), hi.max(new_id)),
                    });
                    protohoggr::encode_bytes_field(&mut s.group, 4, &s.element);
                    total = inc(total)?;
                }
                (1..=3, _) => return Err(foreign_kind_error("relation", field)),
                // Other group fields are dropped (only relations are emitted).
                _ => gr_cursor.skip_field(wire_type).map_err(wire_err)?,
            }
        }
        protohoggr::encode_bytes_field(output, 2, &s.group);
        Ok(())
    })?;
    output.extend_from_slice(&s.scalars);

    Ok(Outcome {
        count: total,
        orphans,
        span: span.get(),
        new_range,
    })
}

/// Rewrite one Relation into `out`. Returns its new ID and orphan count.
#[allow(clippy::too_many_lines)]
fn reframe_relation(
    rel_bytes: &[u8],
    ids: &RelationIdSpaces<'_>,
    span: &mut Span,
    memids_out: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<(i64, u64), String> {
    let mut id: Option<(i64, (usize, usize))> = None;
    let mut memids_range: Option<(usize, usize)> = None;
    let mut old_memids_data: &[u8] = &[];
    let mut types_data: &[u8] = &[];

    let mut cursor = Cursor::new(rel_bytes);
    loop {
        let tag_start = cursor.position();
        let Some((rf, rt)) = cursor.read_tag().map_err(wire_err)? else {
            break;
        };
        match (rf, rt) {
            (1, WIRE_VARINT) => {
                let v = cursor.read_varint_i64().map_err(wire_err)?;
                if id.replace((v, (tag_start, cursor.position()))).is_some() {
                    return Err(repeated_message_field_error("Relation", "id"));
                }
            }
            (9, WIRE_LEN) => {
                let seg = cursor.read_len_delimited().map_err(wire_err)?;
                if !seg.is_empty() && !old_memids_data.is_empty() {
                    return Err(repeated_field_error("Relation", "memids"));
                }
                if memids_range.is_none() || !seg.is_empty() {
                    old_memids_data = seg;
                    memids_range = Some((tag_start, cursor.position()));
                }
            }
            (10, WIRE_LEN) => {
                // Not patched - captured for dispatch. Still guarded: keeping
                // only the last segment silently re-types every member.
                let seg = cursor.read_len_delimited().map_err(wire_err)?;
                if !seg.is_empty() {
                    if !types_data.is_empty() {
                        return Err(repeated_field_error("Relation", "types"));
                    }
                    types_data = seg;
                }
            }
            // The silent-corruption case: an unpacked `memids` left
            // `memids_range` as None, so the splice took the "zero-member
            // relation" branch and copied the member IDs through
            // un-renumbered. An unpacked `types` would leave the dispatch
            // blind instead.
            (9, WIRE_VARINT) => return Err(unpacked_field_error("Relation", "memids")),
            (10, WIRE_VARINT) => return Err(unpacked_field_error("Relation", "types")),
            _ => {
                cursor.read_raw_field(rt).map_err(wire_err)?;
            }
        }
    }

    let (old_rel_id, id_r) =
        id.ok_or_else(|| "reframe_relations: relation without an id field".to_string())?;
    if old_rel_id < 0 {
        return Err(negative_id_error("relation", old_rel_id));
    }
    span.push("relation", old_rel_id)?;
    // R1 recorded every relation id through `relation_id`, which reads the
    // same field this loop does, so a miss is a bug rather than input.
    let new_rel_id = ids.relations.live(old_rel_id)?.ok_or_else(|| {
        format!("renumber: relation {old_rel_id} was not collected by the R1 pass")
    })?;

    // ---- Patch memids: decode old deltas + types, look up new ids ----
    memids_out.clear();
    let mut orphans: u64 = 0;
    if !old_memids_data.is_empty() || !types_data.is_empty() {
        // Both must hold the same number of varints. A full walk rather than
        // a terminator-byte count, which silently miscounts truncated input.
        let memids_count = count_varints_strict(old_memids_data)
            .map_err(|e| format!("reframe_relations: relation {old_rel_id} memids: {e}"))?;
        let types_count = count_varints_strict(types_data)
            .map_err(|e| format!("reframe_relations: relation {old_rel_id} types: {e}"))?;
        if memids_count != types_count {
            return Err(format!(
                "reframe_relations: relation {old_rel_id} has {memids_count} memids \
                 but {types_count} types"
            ));
        }

        let mut memids_cursor = Cursor::new(old_memids_data);
        let mut types_cursor = Cursor::new(types_data);
        let mut prev_old_id: i64 = 0;
        let mut prev_new_id: i64 = 0;
        for _ in 0..memids_count {
            let member_type = types_cursor
                .read_varint()
                .map_err(|e| format!("types varint: {e}"))?;
            let delta = memids_cursor
                .read_sint64()
                .map_err(|e| format!("memids varint: {e}"))?;
            prev_old_id = prev_old_id
                .checked_add(delta)
                .ok_or_else(|| delta_overflow_error("Relation.memids"))?;
            let old_abs_id = prev_old_id;

            // Unconditional reject, same policy as the node and way paths: a
            // negative member ref would otherwise flow through as an orphan.
            if old_abs_id < 0 {
                return Err(format!(
                    "renumber requires non-negative input ids. \
                     Input contains relation {old_rel_id} with \
                     member ref {old_abs_id}. Resolve JOSM-style \
                     negative staging IDs before renumbering.",
                ));
            }

            // `types` is `repeated int32`: dispatch on the varint truncated
            // to 32 bits, as protobuf and pbfhogg's own reader decode it.
            // Matching the raw u64 instead treated 2^32 as an unknown type
            // and kept its ID, which every reader then took as a node ref -
            // to whichever renumbered node now holds that number. The type
            // bytes themselves are copied through unchanged.
            #[allow(clippy::cast_possible_truncation)]
            let member_type = member_type as u32;
            let space = match member_type {
                0 => Some(&ids.nodes),
                1 => Some(&ids.ways),
                2 => Some(&ids.relations),
                _ => None,
            };
            // An unknown type names no ID space renumber reassigns, so its
            // ID cannot alias a renumbered element: preserved, uncounted.
            let (new_abs_id, is_orphan) = match space {
                Some(space) => space
                    .resolve(old_abs_id)
                    .map_err(|e| format!("{e} (in relation {old_rel_id})"))?,
                None => (old_abs_id, false),
            };
            if is_orphan {
                orphans = inc(orphans)?;
            }
            let new_delta = new_abs_id
                .checked_sub(prev_new_id)
                .ok_or_else(|| delta_overflow_error("Relation.memids (output)"))?;
            protohoggr::encode_varint(memids_out, protohoggr::zigzag_encode_64(new_delta));
            prev_new_id = new_abs_id;
        }
    }

    if let Some(memids_r) = memids_range {
        splice_two(
            rel_bytes,
            id_r,
            |o| protohoggr::encode_int64_field(o, 1, new_rel_id),
            memids_r,
            |o| protohoggr::encode_bytes_field(o, 9, memids_out),
            out,
        );
    } else {
        // No memids field (zero-member relation) - only patch id.
        out.clear();
        out.extend_from_slice(&rel_bytes[..id_r.0]);
        protohoggr::encode_int64_field(out, 1, new_rel_id);
        out.extend_from_slice(&rel_bytes[id_r.1..]);
    }
    Ok((new_rel_id, orphans))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            #[allow(clippy::cast_possible_truncation)]
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn len_delimited(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from((field << 3) | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    #[allow(clippy::cast_sign_loss)]
    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    /// Wrap groups into a PrimitiveBlock with an empty StringTable.
    fn block_of(groups: &[Vec<u8>]) -> Vec<u8> {
        let mut block = Vec::new();
        len_delimited(1, &[], &mut block);
        for g in groups {
            len_delimited(2, g, &mut block);
        }
        block
    }

    /// PrimitiveBlock: an empty StringTable plus one group holding one
    /// Relation with id `rel_id` and a single member (node `memid`).
    /// `memids` / `types` are written packed when `packed`, unpacked
    /// otherwise. Both are legal protobuf.
    fn relation_block(rel_id: i64, memid: i64, packed: bool) -> Vec<u8> {
        let mut rel = Vec::new();
        rel.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(rel_id as u64, &mut rel);
        if packed {
            let mut seg = Vec::new();
            varint(zz(memid), &mut seg);
            len_delimited(9, &seg, &mut rel); // memids
        } else {
            rel.push(0x48); // field 9 (memids), VARINT - the unpacked form
            varint(zz(memid), &mut rel);
        }
        // `types` stays packed in BOTH fixtures on purpose: it isolates the
        // memids guard. If types were unpacked too, the sibling types guard
        // would fire first and the message assertion below would pass while
        // testing the wrong thing.
        len_delimited(10, &[0x00], &mut rel); // types: [0] = node

        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group); // PrimitiveGroup.relations
        block_of(&[group])
    }

    fn run(block: &[u8]) -> Result<Outcome, String> {
        let mut node_ids = IdSet::new();
        node_ids.set(7);
        node_ids.build_rank_index();
        let mut rel_ids = IdSet::new();
        rel_ids.set(5);
        rel_ids.build_rank_index();
        let mut way_ids = IdSet::new();
        way_ids.build_rank_index();
        let ids = RelationIdSpaces {
            relations: IdSpace::new("relation", &rel_ids, 1_000),
            nodes: IdSpace::new("node", &node_ids, 100),
            ways: IdSpace::new("way", &way_ids, 500),
        };
        let mut out = Vec::new();
        reframe_relations_with_new_ids(block, &ids, &mut out, &mut Scratch::default())
    }

    /// PrimitiveBlock whose single Relation writes its members across
    /// `memids_segments` occurrences of field 9 and `types_segments`
    /// occurrences of field 10. Both spellings are legal protobuf and mean
    /// exactly the same relation - protobuf concatenates repeated packed
    /// segments.
    fn relation_block_segmented(
        rel_id: i64,
        memids: &[i64],
        memids_segments: usize,
        types_segments: usize,
    ) -> Vec<u8> {
        let mut rel = Vec::new();
        rel.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(rel_id as u64, &mut rel);

        let mut per_member: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &m in memids {
            let mut v = Vec::new();
            varint(zz(m - prev), &mut v);
            prev = m;
            per_member.push(v);
        }
        let chunk = per_member.len().div_ceil(memids_segments.max(1));
        for seg in per_member.chunks(chunk.max(1)) {
            len_delimited(9, &seg.concat(), &mut rel);
        }

        let types: Vec<u8> = vec![0x00; memids.len()];
        let tchunk = types.len().div_ceil(types_segments.max(1));
        for seg in types.chunks(tchunk.max(1)) {
            len_delimited(10, seg, &mut rel);
        }

        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group);
        block_of(&[group])
    }

    /// One Way message: id, then refs split across `segments` occurrences.
    fn way_message(way_id: i64, refs: &[i64], segments: usize) -> Vec<u8> {
        let mut way = Vec::new();
        way.push(0x08);
        #[allow(clippy::cast_sign_loss)]
        varint(way_id as u64, &mut way);

        let mut per_ref: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &r in refs {
            let mut v = Vec::new();
            varint(zz(r.wrapping_sub(prev)), &mut v);
            prev = r;
            per_ref.push(v);
        }
        let chunk = per_ref.len().div_ceil(segments.max(1));
        for seg in per_ref.chunks(chunk.max(1)) {
            len_delimited(8, &seg.concat(), &mut way);
        }
        way
    }

    /// PrimitiveBlock holding one Way whose `refs` are written across
    /// `segments` occurrences of field 8.
    fn way_block_segmented(way_id: i64, refs: &[i64], segments: usize) -> Vec<u8> {
        let mut group = Vec::new();
        len_delimited(3, &way_message(way_id, refs, segments), &mut group);
        block_of(&[group])
    }

    fn run_ways(block: &[u8]) -> Result<Outcome, String> {
        let mut node_ids = IdSet::new();
        node_ids.set(7);
        node_ids.build_rank_index();
        let mut way_ids = IdSet::new();
        let mut out = Vec::new();
        reframe_ways_with_new_ids(
            block,
            1_000,
            &IdSpace::new("node", &node_ids, 100),
            &mut way_ids,
            &mut out,
            &mut Scratch::default(),
        )
    }

    /// Same class as the relation case, and the most damaging of the three:
    /// with two `refs` segments the splice replaced only the last one, so the
    /// earlier segment's node refs were copied through pointing at OLD node
    /// IDs inside a file that claims every ID was renumbered. No error, no
    /// orphan count, nothing.
    #[test]
    fn way_refs_split_across_two_segments_is_refused() {
        let err = run_ways(&way_block_segmented(3, &[7, 9], 2))
            .err()
            .expect("two data-bearing refs segments must be refused");
        assert!(
            err.contains("Way.refs appears twice with data"),
            "expected the Way.refs repeated-occurrence guard specifically - no \
             other guard in this rewriter names Way.refs and a repeated \
             occurrence together, and without it the rewrite SUCCEEDS with \
             half the refs unrenumbered. Got: {err}"
        );
    }

    #[test]
    fn a_single_segment_two_ref_way_is_still_accepted() {
        let outcome = run_ways(&way_block_segmented(3, &[7, 9], 1))
            .expect("one refs segment must be accepted");
        assert_eq!(outcome.count, 1);
        assert_eq!(
            outcome.orphans, 1,
            "node 7 is in the set and node 9 is not, so exactly one ref is an \
             orphan - proving both refs were actually resolved"
        );
        assert_eq!(outcome.span, Some((3, 3)));
    }

    /// PrimitiveBlock with one group per entry of `groups`, each a
    /// DenseNodes whose `id` deltas are written in one packed segment.
    fn dense_groups(groups: &[&[i64]]) -> Vec<u8> {
        let built: Vec<Vec<u8>> = groups
            .iter()
            .map(|ids| {
                let mut packed = Vec::new();
                let mut prev = 0_i64;
                for &id in *ids {
                    varint(zz(id.wrapping_sub(prev)), &mut packed);
                    prev = id;
                }
                let mut dense = Vec::new();
                varint(u64::from((1u32 << 3) | 2), &mut dense);
                varint(packed.len() as u64, &mut dense);
                dense.extend_from_slice(&packed);
                let mut group = Vec::new();
                len_delimited(2, &dense, &mut group);
                group
            })
            .collect();
        block_of(&built)
    }

    /// PrimitiveBlock holding one DenseNodes whose `id` deltas are written
    /// across `segments` occurrences of field 1.
    fn dense_block_segmented(ids: &[i64], segments: usize) -> Vec<u8> {
        let mut per_id: Vec<Vec<u8>> = Vec::new();
        let mut prev = 0_i64;
        for &id in ids {
            let mut v = Vec::new();
            varint(zz(id - prev), &mut v);
            prev = id;
            per_id.push(v);
        }
        let mut dense = Vec::new();
        let chunk = per_id.len().div_ceil(segments.max(1));
        for seg in per_id.chunks(chunk.max(1)) {
            len_delimited(1, &seg.concat(), &mut dense);
        }
        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        block_of(&[group])
    }

    fn run_dense_into(block: &[u8], out: &mut Vec<u8>) -> Result<(Outcome, Vec<i64>), String> {
        let mut ids = IdSet::new();
        ids.pre_allocate(1_000);
        let mut overflow = Vec::new();
        let outcome = reframe_dense_with_new_ids(
            block,
            1,
            &ids,
            &mut overflow,
            out,
            &mut Scratch::default(),
        )?;
        Ok((outcome, overflow))
    }

    fn run_dense(block: &[u8]) -> Result<Outcome, String> {
        run_dense_into(block, &mut Vec::new()).map(|(o, _)| o)
    }

    /// The DenseNodes half. Keeping only the last `id` segment understated the
    /// node count, so every downstream ID base computed from it was wrong and
    /// the earlier segment's nodes were never entered into the node map - they
    /// became orphan refs in every way that pointed at them.
    #[test]
    fn dense_node_ids_split_across_two_segments_are_refused() {
        let err = run_dense(&dense_block_segmented(&[10, 20], 2))
            .err()
            .expect("two data-bearing DenseNodes.id segments must be refused");
        assert!(
            err.contains("DenseNodes.id appears twice with data"),
            "expected the DenseNodes.id repeated-occurrence guard specifically; \
             without it the call SUCCEEDS and simply reports one node instead \
             of two. Got: {err}"
        );
    }

    #[test]
    fn a_single_segment_dense_block_is_still_accepted() {
        let outcome =
            run_dense(&dense_block_segmented(&[10, 20], 1)).expect("one id segment is accepted");
        assert_eq!(
            outcome.count, 2,
            "both node ids must be counted, so the guard cannot have become a \
             refuse-every-dense-block"
        );
        assert_eq!(outcome.span, Some((10, 20)));
    }

    /// A crafted delta that overflows the running id. It used to
    /// panic in debug and wrap in release.
    #[test]
    fn an_overflowing_node_delta_is_an_input_error() {
        let block = dense_groups(&[&[i64::MAX - 1, i64::MAX - 1]]);
        // Deltas: MAX-1, then 0 - harmless. Build an overflow explicitly:
        // first id MAX-1, then delta +5.
        let mut packed = Vec::new();
        varint(zz(i64::MAX - 1), &mut packed);
        varint(zz(5), &mut packed);
        let mut dense = Vec::new();
        len_delimited(1, &packed, &mut dense);
        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        let err = run_dense(&block_of(&[group]))
            .err()
            .expect("overflow must be refused");
        assert!(err.contains("overflows"), "got: {err}");
        // And the zero delta in `block` is a duplicate, refused on order.
        let err = run_dense(&block)
            .err()
            .expect("a duplicate must be refused");
        assert!(err.contains("appears twice"), "got: {err}");
    }

    #[test]
    fn an_overflowing_way_ref_delta_is_an_input_error() {
        let mut way = Vec::new();
        way.push(0x08);
        varint(3, &mut way);
        // The first ref is node 7, which resolves; the second delta then
        // overflows. (A first ref of i64::MAX would be an orphan whose shift
        // overflows before the delta is read.)
        let mut refs = Vec::new();
        varint(zz(7), &mut refs);
        varint(zz(i64::MAX), &mut refs);
        len_delimited(8, &refs, &mut way);
        let mut group = Vec::new();
        len_delimited(3, &way, &mut group);
        let err = run_ways(&block_of(&[group]))
            .err()
            .expect("overflow must be refused");
        assert!(err.contains("Way.refs delta sum overflows"), "got: {err}");
    }

    #[test]
    fn an_overflowing_member_delta_is_an_input_error() {
        let mut rel = Vec::new();
        rel.push(0x08);
        varint(5, &mut rel);
        // As for way refs: member node 7 resolves, then the delta overflows.
        let mut memids = Vec::new();
        varint(zz(7), &mut memids);
        varint(zz(i64::MAX), &mut memids);
        len_delimited(9, &memids, &mut rel);
        len_delimited(10, &[0, 0], &mut rel);
        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group);
        let err = run(&block_of(&[group]))
            .err()
            .expect("overflow must be refused");
        assert!(
            err.contains("Relation.memids delta sum overflows"),
            "got: {err}"
        );
    }

    /// Strictly increasing ids, within a blob: a node id that steps back, across a group
    /// boundary, is refused.
    #[test]
    fn node_ids_must_increase_across_groups_in_a_blob() {
        let err = run_dense(&dense_groups(&[&[10, 20], &[15]]))
            .err()
            .expect("a step back across groups must be refused");
        assert!(err.contains("strictly increase"), "got: {err}");
        let ok = run_dense(&dense_groups(&[&[10, 20], &[25]])).expect("increasing is fine");
        assert_eq!(ok.count, 3);
        assert_eq!(ok.span, Some((10, 25)));
    }

    #[test]
    fn way_ids_must_increase_in_a_blob() {
        let mut group = Vec::new();
        len_delimited(3, &way_message(9, &[7], 1), &mut group);
        len_delimited(3, &way_message(9, &[7], 1), &mut group);
        let err = run_ways(&block_of(&[group]))
            .err()
            .expect("a duplicate way must be refused");
        assert!(err.contains("way 9 appears twice"), "got: {err}");
    }

    /// An empty DenseNodes group used to emit its base id anyway: a
    /// fabricated node, duplicating the next group's first new id, while the
    /// reported count stayed right. Re-reading the rewritten block proves no
    /// id was invented.
    #[test]
    fn an_empty_dense_group_fabricates_no_node() {
        let mut out = Vec::new();
        let (outcome, _) = run_dense_into(&dense_groups(&[&[], &[10]]), &mut out).expect("rewrite");
        assert_eq!(outcome.count, 1);
        let reread = run_dense(&out).expect("the output must itself be a valid node block");
        assert_eq!(
            reread.count, 1,
            "the rewritten block holds exactly one node"
        );

        // The same with an empty group that still carries another field
        // (an empty-node keys_vals). Its empty `id` must survive, or the
        // output is a DenseNodes without an id field, which is refused.
        let mut dense = Vec::new();
        len_delimited(1, &[], &mut dense);
        len_delimited(10, &[0], &mut dense);
        let mut empty_group = Vec::new();
        len_delimited(2, &dense, &mut empty_group);
        let mut node_dense = Vec::new();
        len_delimited(1, &[20], &mut node_dense);
        let mut node_group = Vec::new();
        len_delimited(2, &node_dense, &mut node_group);
        let block = block_of(&[empty_group, node_group]);
        let mut out = Vec::new();
        let (outcome, _) = run_dense_into(&block, &mut out).expect("rewrite");
        assert_eq!(outcome.count, 1);
        let reread = run_dense(&out).expect("the output must itself be a valid node block");
        assert_eq!(reread.count, 1);
    }

    /// Protobuf merges a repeated DenseNodes message; the rewriter used to
    /// keep only the last, dropping the first's nodes with the count check
    /// none the wiser when indexdata agreed.
    #[test]
    fn a_second_dense_nodes_in_one_group_is_refused() {
        let block = dense_groups(&[&[10]]);
        // Re-build with two DenseNodes in the one group.
        let mut dense = Vec::new();
        len_delimited(1, &[20], &mut dense);
        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        len_delimited(2, &dense, &mut group);
        let err = run_dense(&block_of(&[group]))
            .err()
            .expect("must be refused");
        assert!(err.contains("DenseNodes more than once"), "got: {err}");
        assert!(run_dense(&block).is_ok());
    }

    #[test]
    fn a_group_mixing_element_kinds_is_refused_by_every_rewriter() {
        // A way inside a node group: the node rewriter dropped it.
        let mut dense = Vec::new();
        len_delimited(1, &[20], &mut dense);
        let mut group = Vec::new();
        len_delimited(2, &dense, &mut group);
        len_delimited(3, &way_message(3, &[7], 1), &mut group);
        let err = run_dense(&block_of(&[group.clone()]))
            .err()
            .expect("node rewriter");
        assert!(err.contains("also carries Way"), "got: {err}");
        // The same group through the way rewriter: dense nodes used to be
        // copied through un-renumbered.
        let err = run_ways(&block_of(&[group])).err().expect("way rewriter");
        assert!(err.contains("also carries DenseNodes"), "got: {err}");
        // A dense group inside a relation blob: dropped before.
        let mut rel_group = Vec::new();
        len_delimited(2, &dense, &mut rel_group);
        let err = run(&block_of(&[rel_group]))
            .err()
            .expect("relation rewriter");
        assert!(err.contains("also carries DenseNodes"), "got: {err}");
    }

    #[test]
    fn a_second_id_field_is_refused_for_ways_and_relations() {
        let mut way = way_message(3, &[7], 1);
        way.push(0x08);
        varint(4, &mut way);
        let mut group = Vec::new();
        len_delimited(3, &way, &mut group);
        let err = run_ways(&block_of(&[group])).err().expect("way");
        assert!(err.contains("Way carries id more than once"), "got: {err}");

        let mut rel = vec![0x08];
        varint(5, &mut rel);
        rel.push(0x08);
        varint(6, &mut rel);
        assert!(relation_id(&rel).is_err(), "R1's reader must refuse it too");
        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group);
        let err = run(&block_of(&[group])).err().expect("relation");
        assert!(
            err.contains("Relation carries id more than once"),
            "got: {err}"
        );
    }

    /// The relation half of the pre-allocation overflow-path contract does not
    /// apply, but the node half does: an id the pre-allocation does not
    /// cover is handed back rather than refused.
    #[test]
    fn a_node_id_past_the_preallocation_goes_to_overflow() {
        let (outcome, overflow) =
            run_dense_into(&dense_groups(&[&[5, 1 << 40]]), &mut Vec::new()).expect("rewrite");
        assert_eq!(outcome.count, 2);
        assert_eq!(overflow, vec![1 << 40]);
    }

    #[test]
    fn relation_memids_split_across_two_segments_is_refused() {
        let err = run(&relation_block_segmented(5, &[7, 9], 2, 1))
            .err()
            .expect("two data-bearing memids segments must be refused");
        assert!(
            err.contains("Relation.memids appears twice with data"),
            "expected the memids repeated-occurrence guard specifically. Only \
             that guard names Relation.memids together with a repeated \
             occurrence; without it the sibling memids/types count cross-check \
             fires instead, with a misleading message about a member count \
             mismatch. Got: {err}"
        );
    }

    /// `types` is not spliced - it is only read for the per-member dispatch -
    /// so keeping the last segment gives a member array typed against the
    /// wrong ID spaces, or a spurious count mismatch. Guarded too.
    #[test]
    fn relation_types_split_across_two_segments_is_refused() {
        let err = run(&relation_block_segmented(5, &[7, 9], 1, 2))
            .err()
            .expect("two data-bearing types segments must be refused");
        assert!(
            err.contains("Relation.types appears twice with data"),
            "expected the types repeated-occurrence guard specifically; the \
             count cross-check would otherwise claim a memids/types length \
             mismatch. Got: {err}"
        );
    }

    /// Passing counterpart for both guards: the single-segment spelling of the
    /// same two-member relation must still be accepted and renumbered. A
    /// refuse-every-relation regression fails here.
    #[test]
    fn a_single_segment_two_member_relation_is_still_accepted() {
        let outcome = run(&relation_block_segmented(5, &[7, 9], 1, 1))
            .expect("one memids segment and one types segment must be accepted");
        assert_eq!(outcome.count, 1);
        assert_eq!(
            outcome.orphans, 1,
            "node 7 is in the set and node 9 is not, so exactly one member is \
             an orphan - proving both members were actually walked"
        );
        assert_eq!(outcome.new_range, Some((1_000, 1_000)));
    }

    fn ranked(ids: &[i64]) -> IdSet {
        let mut set = IdSet::new();
        for &id in ids {
            set.set(id);
        }
        set.build_rank_index();
        set
    }

    /// The collision BUG-071 names: old ids 1 and 2 are orphans here and
    /// are also the new ids of nodes 10 and 20. Shifted by start + count
    /// they land past the renumbered range, distinct from every live id.
    #[test]
    fn an_orphan_is_shifted_past_the_renumbered_range() {
        let set = ranked(&[10, 20]);
        let nodes = IdSpace::new("node", &set, 1);
        assert_eq!(nodes.resolve(10).unwrap(), (1, false));
        assert_eq!(nodes.resolve(20).unwrap(), (2, false));
        assert_eq!(nodes.resolve(1).unwrap(), (4, true));
        assert_eq!(nodes.resolve(2).unwrap(), (5, true));
        assert_eq!(
            nodes.resolve(0).unwrap(),
            (3, true),
            "orphan 0 maps to the base"
        );
        assert_eq!(nodes.live(1).unwrap(), None);
    }

    /// A live range may end exactly at i64::MAX; only an orphan that would
    /// need an id past it is an error.
    #[test]
    fn a_range_ending_at_the_largest_id_fails_only_on_an_orphan() {
        let set = ranked(&[7]);
        let nodes = IdSpace::new("node", &set, i64::MAX);
        assert_eq!(nodes.resolve(7).unwrap(), (i64::MAX, false));
        let err = nodes.resolve(0).expect_err("orphan 0 needs i64::MAX + 1");
        assert!(err.contains("overflows a 64-bit id"), "got: {err}");
        assert!(err.contains("orphan node ref 0"), "got: {err}");

        let set = ranked(&[7]);
        let ways = IdSpace::new("way", &set, i64::MAX - 10);
        // Base i64::MAX - 9: orphan 9 is exactly i64::MAX, orphan 10 is past it.
        assert_eq!(ways.resolve(9).unwrap(), (i64::MAX, true));
        assert!(ways.resolve(10).is_err());
    }

    /// PrimitiveBlock with relation 5 whose members are `(raw type, id)`.
    fn relation_block_typed(members: &[(u64, i64)]) -> Vec<u8> {
        let mut rel = vec![0x08];
        varint(5, &mut rel);
        let mut memids = Vec::new();
        let mut types = Vec::new();
        let mut prev = 0_i64;
        for &(t, id) in members {
            varint(zz(id - prev), &mut memids);
            prev = id;
            varint(t, &mut types);
        }
        len_delimited(9, &memids, &mut rel);
        len_delimited(10, &types, &mut rel);
        let mut group = Vec::new();
        len_delimited(4, &rel, &mut group);
        block_of(&[group])
    }

    /// `types` is int32: a raw 2^32 is read back as Node by pbfhogg's reader,
    /// so renumber must resolve it as one. Matched on the raw u64 it was an
    /// "unknown" type whose id was kept, and so aliased the renumbered node
    /// with that number.
    #[test]
    fn member_types_are_dispatched_as_int32() {
        // Node 7 is live (-> 100), node 9 is an orphan.
        let outcome = run(&relation_block_typed(&[(1 << 32, 7), (1 << 32, 9)])).unwrap();
        assert_eq!(
            outcome.orphans, 1,
            "2^32 is a node type, so node 9 is an orphan"
        );
        // 2^32 + 2 is a relation: relation 5 is live, relation 6 is not.
        let outcome = run(&relation_block_typed(&[
            ((1 << 32) | 2, 5),
            ((1 << 32) | 2, 6),
        ]))
        .unwrap();
        assert_eq!(outcome.orphans, 1);
        // Genuinely unknown types (3, and u32::MAX, which is int32 -1) are
        // preserved and not counted.
        let outcome = run(&relation_block_typed(&[(3, 9), (u64::from(u32::MAX), 11)])).unwrap();
        assert_eq!(outcome.orphans, 0);
    }

    #[test]
    fn unpacked_relation_memids_are_rejected_not_silently_passed_through() {
        // This is the silent-corruption case. With `memids` unpacked, the
        // splice rewriter found no `memids_range`, took its "zero-member
        // relation" branch, patched only the id, and copied the member IDs
        // through UN-RENUMBERED - into output that claims to be renumbered.
        let err = run(&relation_block(5, 7, false))
            .err()
            .expect("an unpacked memids field must be refused, not mis-spliced");
        assert!(
            err.contains("Relation.memids is written in the unpacked wire form"),
            "expected the memids unpacked-form guard specifically; only that \
             guard names Relation.memids and the unpacked form together. Got: {err}"
        );

        // Passing counterpart: the packed spelling of the identical relation
        // must succeed and actually renumber the member. node 7 is the only
        // node in the set, so rank 0 -> new id 100.
        let outcome = run(&relation_block(5, 7, true)).expect("the packed form must be accepted");
        assert_eq!(outcome.count, 1);
        assert_eq!(
            outcome.orphans, 0,
            "node 7 is present, so its ref is not an orphan"
        );
    }
}
