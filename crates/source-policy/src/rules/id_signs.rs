//! An OSM ID's sign is decided only where an inventoried reason says so.
//!
//! Negative IDs are handled like any other ID: a command that compares an
//! ID against zero, casts it to an unsigned type, or takes its magnitude
//! can drop, fold or misplace a negative one silently, which is how every
//! negative-ID bug fixed so far was spelled (`id >= 0` guards before an
//! insertion, `|&r| r >= 0` over way refs, a `start < 1` refusal). So every
//! such operation on a recognisable ID in production code under
//! `crates/lib/src/` and `crates/cli/src/` must match an [`INVENTORY`]
//! entry: (file, owning item, the operation as written, exact count, and
//! a reason saying what happens to IDs of both signs there). The written
//! operation runs from the start of its statement, argument or element
//! (after the nearest `;`, `{`, `}`, `,`, open delimiter or attribute)
//! to the end of its expression or condition (before the next `{`, `;`,
//! `,` or `=>` at its depth), whitespace read as one space, so it carries
//! the receiver, the operator, the threshold, the rest of the condition
//! and what the result feeds: `min_id > 0` turned into `min_id >= 0`,
//! `claim.min_id` into `other.min_id`, `r.negative |= id < 0` into
//! `if id < 0`, or a dropped `&& idx.min_id > target_id` is a finding even
//! where the count stays. The blocks a condition guards are outside the
//! key: swapping what two branches do passes it, and is the reviewer's. An unlisted operation, a count that moved,
//! an entry that matches nothing, an entry without a reason, and an
//! operation whose owner names no single declaration (two impls of one
//! type) are all findings.
//!
//! Recognised operands: a name `id`, ending in `_id`, or one of
//! [`ID_NAMES`] (global names, not bindings: a lexer cannot follow scope
//! soundly, and a non-ID hit costs only an inventory line), as a plain
//! name, the last field of a chain, a call so named (`node.id()`,
//! `element_id(e)`) or one parenthesised group around one of those, and not
//! inside arithmetic (`base + id < 0` compares the sum, `id & mask` a
//! mask). Recognised operations: a comparison with 0, 1 or -1 on either
//! side; `as` an
//! unsigned type; the magnitude and sign methods in [`SIGN_METHODS`], as a
//! method or an associated call (`i64::abs(id)`); `.max(0)`, `.min(0)` and
//! `.clamp(0, ..)`; and `uN::try_from(id)`. Test-only code (the gates the
//! library text rule recognises) is outside the rule: a fixture's sign is
//! not a product answer. A file is judged on its own gates: one mounted by
//! a test-gated `mod` declaration needs its own `#![cfg(test)]`.
//!
//! A review backstop, not proof. It has no types: an ID under another name,
//! a sign decided through a helper, arithmetic before the comparison,
//! `try_into()`, a bound seeded at zero rather than `i64::MIN`, and macro
//! input whose tokens do not spell these shapes all pass it; a const
//! generic argument spelled `name_id<0, ..>` would be read as a
//! comparison. The signed `IdSet` and the negative-ID test registry carry
//! the rest.

use std::collections::BTreeMap;

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

const TREES: [&str; 2] = ["crates/lib/src/", "crates/cli/src/"];

/// Names that hold an OSM ID beyond `id` and `*_id`: way and node
/// shorthands, a way's node refs, renumber's start and a parent relation.
pub const ID_NAMES: &[&str] = &["wid", "nid", "rid", "nref", "r", "start", "parent"];

/// Words before a parenthesis that open a group, not a call.
const KEYWORDS: &[&str] = &[
    "if", "while", "match", "return", "in", "let", "else", "for", "move", "break", "as",
];

const UNSIGNED: &[&str] = &["u8", "u16", "u32", "u64", "u128", "usize"];
const SIGNED: &[&str] = &["i8", "i16", "i32", "i64", "i128", "isize"];

/// Methods that take a magnitude or read a sign.
pub const SIGN_METHODS: &[&str] = &[
    "abs",
    "unsigned_abs",
    "wrapping_abs",
    "checked_abs",
    "saturating_abs",
    "overflowing_abs",
    "cast_unsigned",
    "is_negative",
    "is_positive",
    "signum",
];

/// One allowed sign operation: (file, owning item path, operation as
/// written from its statement start, count, why it is right for both
/// signs).
pub struct Entry {
    pub file: &'static str,
    pub owner: &'static str,
    pub site: &'static str,
    pub count: usize,
    pub reason: &'static str,
}

const fn e(
    file: &'static str,
    owner: &'static str,
    site: &'static str,
    count: usize,
    reason: &'static str,
) -> Entry {
    Entry {
        file,
        owner,
        site,
        count,
        reason,
    }
}

const CANONICAL: &str = "canonical order (zero, then negatives by magnitude, then positives) \
                         keys each sign's family distinctly; every ID gets a key";
const BLOB_RANGE: &str = "a blob's numeric [min_id, max_id] claim is mapped per sign case (all \
                          non-negative, all non-positive, straddling zero) to a canonical bound: \
                          the last key exact, the first a conservative lower bound (zero for a \
                          straddling blob), present_first the earliest key the extrema prove \
                          present; every case answers and consumers use each in its direction";
const EXT_FAMILY: &str = "external ALTW keys IDs <= 0 by magnitude and IDs > 0 by themselves in \
                          two bucket families when a node claim is negative; both families join";
const NUMERIC_SET: &str = "the signed IdSet keeps non-negatives by id and negatives by !id in two \
                           bitmaps; a numeric range or walk visits both";
const REPORT: &str = "the has_negative_ids report field observes whether an element ID of the \
                      selected kinds is negative; nothing is selected by it";
const NOT_AN_ID: &str = "not an OSM ID: a worker index or a string-table handle, never signed data";
const BITS: &str = "the full two's-complement bit pattern is the value (hashing or paging), so \
                    distinct IDs of either sign stay distinct";
const RENUMBER: &str = "renumber counts up from a positive start and down from a negative one \
                        (0 means 1); every ID of either sign is assigned";

pub const INVENTORY: &[Entry] = &[
    // The one sign decision of the signed set.
    // The signed set's backend keys.
    e(
        "crates/lib/src/idset.rs",
        "NonNegId::new",
        "(id >= 0).then_some(Self(id as u64))",
        1,
        "the checked constructor of the non-negative backend key: it returns a key only for id >= 0 (the cast beside it is lossless for every key it returns); its callers handle the None (pre_allocate ignores a negative bound, the atomic path reports it uncovered for the caller to spill to insert)",
    ),
    e(
        "crates/lib/src/idset.rs",
        "NonNegId::new",
        "id as u64",
        1,
        "the cast is evaluated for every input but escapes only in the key returned for id >= 0, where it is lossless",
    ),
    e(
        "crates/lib/src/idset.rs",
        "family",
        "if id >= 0",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "family",
        "id as u64",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::chunk_for_atomic",
        "if id < 0",
        1,
        "picks the panic diagnostic of the panicking atomic setters, which a caller reaches only for pre-allocated non-negative IDs; the fallible setters bypass it and report a negative uncovered for the caller to spill to insert",
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::chunk_for_atomic",
        "(id as u64) >> ID_SHIFT",
        1,
        "a chunk number for the non-negative panic diagnostic, after the id < 0 branch",
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::any_in_numeric_range",
        "if min_id < 0",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::any_in_numeric_range",
        "max_id >= 0 && self.nonneg.any_in(NonNegId(min_id.max(0) as u64), NonNegId(max_id as u64))",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::any_in_numeric_range",
        "min_id.max(0) as u64",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::any_in_numeric_range",
        "max_id as u64",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::iter_numeric_from",
        "let neg_last = (start < 0).then_some(NonNegId((!start) as u64))",
        1,
        NUMERIC_SET,
    ),
    e(
        "crates/lib/src/idset.rs",
        "IdSet::iter_numeric_from",
        "start.max(0) as u64",
        1,
        NUMERIC_SET,
    ),
    // Canonical order.
    e(
        "crates/lib/src/osm_id.rs",
        "osm_id_key",
        "if id > 0",
        1,
        CANONICAL,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "osm_id_key",
        "id.unsigned_abs()",
        2,
        CANONICAL,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "osm_id_ordinal",
        "if id > 0",
        1,
        CANONICAL,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "osm_id_ordinal",
        "(id as u64) | (1 << 63)",
        1,
        CANONICAL,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "osm_id_ordinal",
        "id.unsigned_abs()",
        1,
        CANONICAL,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_first_key",
        "if min_id >= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_first_key",
        "else if max_id <= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_last_key",
        "if min_id >= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_last_key",
        "else if max_id <= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_first_id",
        "if min_id >= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_first_id",
        "else if max_id <= 0",
        1,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_last_id",
        "if max_id <= 0 && min_id < 0",
        2,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/osm_id.rs",
        "blob_osm_present_first_key",
        "if min_id < 0 && max_id > 0",
        2,
        BLOB_RANGE,
    ),
    e(
        "crates/lib/src/commands/apply_changes/refresh.rs",
        "find",
        "if id > 0",
        1,
        "the probe vector is sorted by osm_id_cmp, so positives form its tail; a positive is searched there and an ID <= 0 in the head, both exactly",
    ),
    e(
        "crates/lib/src/commands/apply_changes/refresh.rs",
        "ProbeNodes::new",
        "|&id| id <= 0",
        2,
        "finds where the positive tail of an osm_id_cmp-sorted vector starts, for find",
    ),
    e(
        "crates/lib/src/commands/apply_changes/refresh.rs",
        "probe_hash",
        "id.cast_unsigned().wrapping_mul(0x9E37_79B9_7F4A_7C15)",
        1,
        BITS,
    ),
    // Non-negative fast paths with a signed fallback.
    e(
        "crates/lib/src/blob_meta/tally.rs",
        "AscendingTally::holds",
        "if claim.min_id < 0",
        1,
        "the ascending tally decides non-negative claims only; a negative claim is an internal error because hold_claim routes it to the canonical claim walk first",
    ),
    e(
        "crates/lib/src/blob_meta/tally.rs",
        "hold_claim",
        "if claim.min_id >= 0",
        1,
        "non-negative claims take the ascending tally fast path; any other claim is decided by the canonical claim walk",
    ),
    // External ALTW families.
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "Family::key",
        "Self::Pos => id as u64",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "Family::key",
        "Self::Neg => id.unsigned_abs()",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "self.signed |= min_id < 0",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "if let Ok(m) = u64::try_from(max_id)",
        1,
        "the one-family layout's non-negative maximum (zero included); the signed layout takes its positive maximum from max_id > 0 and its non-positive magnitude from min_id below, so a negative max_id is accounted there",
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "if max_id > 0",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "max_id.unsigned_abs()",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "if min_id > 0",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/keys.rs",
        "NodeClaims::observe",
        "min_id.unsigned_abs()",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/blob_meta.rs",
        "scan_blob_metadata",
        "let split = if matches!(idx.kind, ElementKind::Node) && idx.min_id < 0 && idx.max_id > 0",
        2,
        "only a node blob straddling zero is decoded for each family's span; the others are read from their claims",
    ),
    e(
        "crates/lib/src/commands/altw/external/blob_meta.rs",
        "BoundaryDecoder::decode",
        "let part = if id > 0",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/blob_meta.rs",
        "BoundaryDecoder::decode",
        "let key = id.unsigned_abs()",
        1,
        EXT_FAMILY,
    ),
    e(
        "crates/lib/src/commands/altw/external/coord_payloads.rs",
        "ConcurrentBlobLocationRouter::pread_blob_payload",
        "worker_id as usize",
        1,
        NOT_AN_ID,
    ),
    e(
        "crates/lib/src/commands/altw/external/stage3.rs",
        "stage3_slot_reorder",
        "worker_id as u32",
        2,
        NOT_AN_ID,
    ),
    e(
        "crates/lib/src/commands/altw/external/stage3.rs",
        "emit_integrated_intersections",
        "worker_id as usize",
        1,
        NOT_AN_ID,
    ),
    e(
        "crates/lib/src/osc/interner.rs",
        "StringTable::resolve",
        "id as usize",
        1,
        NOT_AN_ID,
    ),
    // Signed-numeric keys and bit patterns.
    e(
        "crates/lib/src/commands/check/locations/mod.rs",
        "id_key",
        "(id as u64) ^ (1 << 63)",
        1,
        "the sign bit is flipped after the cast, so u64 order is signed numeric order over all of i64",
    ),
    e(
        "crates/lib/src/commands/degrade/mod.rs",
        "drop_hash",
        "let idw = id as u64",
        1,
        BITS,
    ),
    e(
        "crates/lib/src/commands/extract/membership.rs",
        "split",
        "let bits = id.cast_unsigned()",
        1,
        BITS,
    ),
    // Reports.
    e(
        "crates/lib/src/commands/check/verify_ids.rs",
        "KindJudge::push",
        "r.negative |= id < 0",
        1,
        REPORT,
    ),
    e(
        "crates/lib/src/commands/inspect/scan.rs",
        "finish_id_stats",
        "|r| r.has_data() && r.min_id < 0",
        1,
        REPORT,
    ),
    e(
        "crates/lib/src/commands/inspect/project.rs",
        "InspectProjection::observe_negative_id",
        "ext.has_negative_ids |= id < 0",
        1,
        REPORT,
    ),
    e(
        "crates/lib/src/commands/inspect/project.rs",
        "InspectProjection::dense_negative_ids",
        "id < 0",
        1,
        REPORT,
    ),
    // Reservation coverage.
    e(
        "crates/lib/src/commands/tags_filter/mod.rs",
        "publish_way_refs",
        "if id < 0 || id > max || !batch.insert(id)",
        1,
        "the atomic reservation covers 0..=max; a negative ref or one above max goes to the signed spill set, so both signs reach the same membership",
    ),
    // Renumber's direction.
    e(
        "crates/lib/src/commands/renumber/mod.rs",
        "positional_bases",
        "let up = start > 0",
        1,
        RENUMBER,
    ),
    e(
        "crates/lib/src/commands/renumber/wire_rewrite.rs",
        "offset_id",
        "if start > 0",
        1,
        RENUMBER,
    ),
    e(
        "crates/lib/src/commands/renumber/wire_rewrite.rs",
        "reframe_dense_with_new_ids",
        "let step = if base_new_id > 0",
        1,
        "the dense delta step is +1 counting up and -1 counting down; both directions are written",
    ),
];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    errors_with(corpus, INVENTORY)
}

/// Every finding over `corpus` against `inventory`.
pub fn errors_with(corpus: &Corpus, inventory: &[Entry]) -> Vec<String> {
    let mut found = Vec::new();
    for entry in inventory {
        if entry.reason.trim().is_empty() {
            found.push(format!(
                "ID sign inventory entry ({}, {}, `{}`) gives no reason",
                entry.file, entry.owner, entry.site
            ));
        }
    }
    // (file, owner, context) -> lines, shape
    let mut seen: BTreeMap<(String, String, String), (Vec<usize>, String)> = BTreeMap::new();
    let mut ambiguous: BTreeMap<(String, String), usize> = BTreeMap::new();
    for tree in TREES {
        found.extend(super::require_files(corpus, tree, "ID signs"));
        for (path, source) in corpus.under(tree) {
            let hits = sites(source);
            if hits.is_empty() {
                continue;
            }
            let file = match syn::parse_file(&source.text) {
                Ok(file) => file,
                Err(e) => {
                    found.push(format!(
                        "{path}: does not parse ({e}), so its ID sign operations cannot be owned"
                    ));
                    continue;
                }
            };
            let gated = super::library_text::test_gated(source, &file);
            let owners = super::Owners::of(source, &file);
            for hit in hits {
                if gated.iter().any(|r| r.contains(&hit.offset)) {
                    continue;
                }
                let owner = owners.owner(hit.offset).to_owned();
                if owners.declarations(&owner) != 1 {
                    ambiguous.insert(
                        (path.to_owned(), owner.clone()),
                        owners.declarations(&owner),
                    );
                }
                let slot = seen
                    .entry((path.to_owned(), owner, hit.context))
                    .or_insert_with(|| (Vec::new(), hit.shape));
                slot.0.push(hit.line);
            }
        }
    }
    for ((path, owner), n) in &ambiguous {
        found.push(format!(
            "{path}: an ID sign operation in `{owner}`, which names {n} declarations (or none); \
             move it into an item of its own so an inventory entry can name it"
        ));
    }
    let mut used = vec![false; inventory.len()];
    for ((path, owner, site), (lines, shape)) in &seen {
        let entry = inventory
            .iter()
            .position(|e| e.file == path && e.owner == owner && e.site == site);
        match entry {
            Some(i) => {
                used[i] = true;
                if inventory[i].count != lines.len() {
                    found.push(format!(
                        "{path}:{}: `{site}` occurs {} times in `{owner}`, the ID sign inventory \
                         says {}; a sign operation was added or removed, so re-read the reason",
                        lines[0],
                        lines.len(),
                        inventory[i].count
                    ));
                }
            }
            None => found.push(format!(
                "{path}:{}: `{shape}` in `{owner}` (`{site}`) decides an OSM ID's sign; \
                 negative IDs are handled like any other, so drop the sign decision, or add an \
                 ID sign inventory entry (crates/source-policy/src/rules/id_signs.rs) saying \
                 what happens to both signs",
                lines[0]
            )),
        }
    }
    for (entry, used) in inventory.iter().zip(used) {
        if !used {
            found.push(format!(
                "ID sign inventory entry ({}, {}, `{}`) matches nothing; remove it",
                entry.file, entry.owner, entry.site
            ));
        }
    }
    found
}

/// One sign operation on a recognisable ID.
#[derive(Debug, PartialEq, Eq)]
pub struct Site {
    pub line: usize,
    pub offset: usize,
    /// Normalised: the operand's ID name, the operator and its argument.
    pub shape: String,
    /// The inventory key: the operation as written, from the start of its
    /// statement, argument or element (receiver and consumer included).
    pub context: String,
}

fn is_id_name(name: &str) -> bool {
    name == "id" || name.ends_with("_id") || ID_NAMES.contains(&name)
}

struct Toks<'a> {
    source: &'a Source,
    code: Vec<Tok>,
}

impl Toks<'_> {
    fn punct(&self, j: usize, p: &str) -> bool {
        self.code.get(j).is_some_and(|t| self.source.is_punct(t, p))
    }
    fn name(&self, j: usize) -> Option<&str> {
        self.code.get(j).and_then(|t| self.source.name(t))
    }
    fn joined(&self, a: usize, b: usize) -> bool {
        matches!((self.code.get(a), self.code.get(b)), (Some(x), Some(y)) if x.end == y.start)
    }

    /// The matching close of the delimiter opening at `open`.
    fn close_of(&self, open: usize) -> Option<usize> {
        let (o, c) = match self.code.get(open).map(|t| self.source.text_of(t)) {
            Some("(") => ("(", ")"),
            Some("[") => ("[", "]"),
            _ => return None,
        };
        let mut depth = 0_usize;
        for j in open..self.code.len() {
            if self.punct(j, o) {
                depth += 1;
            } else if self.punct(j, c) {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
        }
        None
    }

    /// The matching open of the delimiter closing at `close`.
    fn open_of(&self, close: usize) -> Option<usize> {
        let (o, c) = match self.code.get(close).map(|t| self.source.text_of(t)) {
            Some(")") => ("(", ")"),
            Some("]") => ("[", "]"),
            _ => return None,
        };
        let mut depth = 0_usize;
        for j in (0..=close).rev() {
            if self.punct(j, c) {
                depth += 1;
            } else if self.punct(j, o) {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
        }
        None
    }

    /// The ID name of the operand ending at `end`, read backwards: a name
    /// (the last field of any chain), an empty call `name()`, or one
    /// parenthesised group around either.
    fn operand_ending(&self, end: usize) -> Option<String> {
        if let Some(name) = self.name(end) {
            if self.punct(end + 1, "(") || (self.punct(end + 1, ":") && self.punct(end + 2, ":")) {
                return None;
            }
            return is_id_name(name).then(|| name.to_owned());
        }
        if !self.punct(end, ")") {
            return None;
        }
        let open = self.open_of(end)?;
        let callee = open
            .checked_sub(1)
            .and_then(|j| self.name(j))
            .filter(|n| !KEYWORDS.contains(n));
        if let Some(name) = callee {
            // A call: its value is an ID when its name says so.
            return is_id_name(name).then(|| name.to_owned());
        }
        if end > open + 1 {
            let inner = self.operand_ending(end - 1)?;
            // The group holds exactly that operand.
            return (self.operand_start(open + 1) == Some(end - 1)).then_some(inner);
        }
        None
    }

    /// Where the chain of names, fields, paths and calls ending at `end`
    /// begins.
    fn operand_begin(&self, mut end: usize) -> usize {
        loop {
            let mut at = end;
            if self.punct(at, ")") || self.punct(at, "]") {
                let Some(open) = self.open_of(at) else {
                    return at;
                };
                at = open;
                if open > 0 && self.name(open - 1).is_some_and(|n| !KEYWORDS.contains(&n)) {
                    at = open - 1;
                }
            }
            if at >= 2 && self.punct(at - 1, ".") {
                end = at - 2;
            } else if at >= 3 && self.punct(at - 1, ":") && self.punct(at - 2, ":") {
                end = at - 3;
            } else {
                return at;
            }
        }
    }

    /// Whether a reversed comparison's threshold at `at` starts an operand
    /// (rather than ending an arithmetic expression such as `x - 0 < id`).
    fn opens_operand(&self, at: usize) -> bool {
        let Some(j) = at.checked_sub(1) else {
            return true;
        };
        let Some(b) = self.code.get(j) else {
            return true;
        };
        if ["if", "while", "return", "match", "in"]
            .iter()
            .any(|w| self.source.is_word(b, w))
        {
            return true;
        }
        if b.kind != Kind::Punct {
            return false;
        }
        match self.source.text_of(b) {
            "(" | "[" | "{" | ";" | "," | "=" | "!" => true,
            // `=>`, `&&`, `||` and the bars of a closure.
            ">" => j > 0 && self.punct(j - 1, "=") && self.joined(j - 1, j),
            "&" => j > 0 && self.punct(j - 1, "&") && self.joined(j - 1, j),
            "|" => !self.bitwise_or(j),
            _ => false,
        }
    }

    /// Whether the token before `begin` makes the operand part of an
    /// arithmetic expression (`base + id < 0` compares the sum).
    fn in_arithmetic(&self, begin: usize) -> bool {
        let Some(j) = begin.checked_sub(1) else {
            return false;
        };
        if ["+", "-", "*", "/", "%", "^"]
            .iter()
            .any(|p| self.punct(j, p))
        {
            return true;
        }
        // A single `&` is a bitwise and; `&&` and a reference are not.
        if self.punct(j, "&") {
            return !(j > 0 && self.punct(j - 1, "&") && self.joined(j - 1, j))
                && j > 0
                && (self.name(j - 1).is_some() || self.punct(j - 1, ")"));
        }
        self.bitwise_or(j)
    }

    /// Whether the `|` at `j` is a bitwise or: not half of `||` and not
    /// the closing bar of a closure's parameter list.
    fn bitwise_or(&self, j: usize) -> bool {
        if !self.punct(j, "|") {
            return false;
        }
        let paired = (j > 0 && self.punct(j - 1, "|") && self.joined(j - 1, j))
            || (self.punct(j + 1, "|") && self.joined(j, j + 1));
        !paired && !self.closes_closure_params(j)
    }

    /// Whether the `|` at `j` closes `|params|` opened where a closure can
    /// start (after `(`, `,`, `=`, `{`, `;`, `move` or `return`).
    fn closes_closure_params(&self, j: usize) -> bool {
        let mut k = j;
        while k > 0 {
            k -= 1;
            if self.punct(k, "|") {
                return k == 0
                    || self.code.get(k - 1).is_some_and(|b| {
                        ["(", ",", "=", "{", ";"]
                            .iter()
                            .any(|p| self.source.is_punct(b, p))
                            || self.source.is_word(b, "move")
                            || self.source.is_word(b, "return")
                    });
            }
            let param = self.name(k).is_some()
                || ["&", ":", ",", "(", ")", "<", ">", "[", "]"]
                    .iter()
                    .any(|p| self.punct(k, p));
            if !param {
                return false;
            }
        }
        false
    }

    /// The source of the operation from the start of its enclosing
    /// statement, argument or element through `last`, tokens joined as
    /// written with any whitespace read as one space.
    fn context(&self, first: usize, last: usize) -> String {
        let mut s = first;
        while s > 0 {
            let j = s - 1;
            if self.punct(j, ")") || self.punct(j, "]") {
                let Some(open) = self.open_of(j) else {
                    break;
                };
                // An attribute ends the context: `#[...]` is not the statement.
                if self.punct(j, "]")
                    && open.checked_sub(1).is_some_and(|h| {
                        self.punct(h, "#")
                            || (h > 0 && self.punct(h, "!") && self.punct(h - 1, "#"))
                    })
                {
                    break;
                }
                s = open;
                continue;
            }
            // A grouping parenthesis is part of the expression: step out of
            // it, so `(id as u64) ^ (1 << 63)` is keyed whole. A call's or
            // macro's parenthesis ends the context at its argument.
            let callee = j.checked_sub(1).is_some_and(|h| {
                self.name(h).is_some_and(|n| !KEYWORDS.contains(&n))
                        // `name!(..)` is a macro; a bare `!(..)` negates a group.
                        || (self.punct(h, "!") && h > 0 && self.name(h - 1).is_some())
            });
            if self.punct(j, "(") && !callee {
                s = j;
                continue;
            }
            if [";", "{", "}", ",", "(", "["]
                .iter()
                .any(|p| self.punct(j, p))
            {
                break;
            }
            s = j;
        }
        let last = self.context_end(s, last);
        let mut text = String::new();
        for k in s..=last {
            // rustfmt breaks a chain before its `.`; no space there keeps
            // the key the same on one line or many.
            if k > s && !self.joined(k - 1, k) && !self.punct(k, ".") && !self.punct(k - 1, ".") {
                text.push(' ');
            }
            text.push_str(self.source.text_of(&self.code[k]));
        }
        text
    }

    /// Where the operand starting at `start` ends, reading forwards over a
    /// chain of names, fields, paths and calls; `None` for anything else.
    fn operand_start(&self, mut at: usize) -> Option<usize> {
        while self.punct(at, "&") || self.punct(at, "*") {
            at += 1;
        }
        let mut end;
        if self.punct(at, "(") {
            end = self.close_of(at)?;
        } else if self.name(at).is_some() {
            end = at;
        } else {
            return None;
        }
        loop {
            if self.punct(end + 1, "(") || self.punct(end + 1, "[") {
                end = self.close_of(end + 1)?;
            } else if self.punct(end + 1, ".")
                && (self.name(end + 2).is_some()
                    || self.code.get(end + 2).is_some_and(|t| t.kind == Kind::Int))
            {
                // A field, a method name or a tuple index.
                end += 2;
            } else if self.punct(end + 1, ":")
                && self.punct(end + 2, ":")
                && self.name(end + 3).is_some()
            {
                end += 3;
            } else {
                return Some(end);
            }
        }
    }

    /// A threshold literal `0`, `1` or `-1` starting at `at`: (text, last
    /// token index).
    fn threshold(&self, at: usize) -> Option<(String, usize)> {
        let (neg, lit) = if self.punct(at, "-") {
            (true, at + 1)
        } else {
            (false, at)
        };
        let tok = self.code.get(lit)?;
        if tok.kind != Kind::Int {
            return None;
        }
        let value = int_value(self.source.text_of(tok))?;
        match (neg, value) {
            (false, 0) => Some(("0".to_owned(), lit)),
            (false, 1) => Some(("1".to_owned(), lit)),
            (true, 1) => Some(("-1".to_owned(), lit)),
            _ => None,
        }
    }

    /// A comparison operator at `at`: (text, last token index). `<`/`>`
    /// alone or with a joined `=`, never part of `<<`, `>>`, `->`, `=>`,
    /// `<=` read from its `=`, or `>>=`.
    fn comparison(&self, at: usize) -> Option<(&'static str, usize)> {
        let lt = self.punct(at, "<");
        let gt = self.punct(at, ">");
        if !lt && !gt {
            return None;
        }
        if at > 0
            && self.joined(at - 1, at)
            && (self.punct(at - 1, "<")
                || self.punct(at - 1, ">")
                || self.punct(at - 1, "-")
                || self.punct(at - 1, "="))
        {
            return None;
        }
        if self.joined(at, at + 1) && (self.punct(at + 1, "<") || self.punct(at + 1, ">")) {
            return None;
        }
        if self.joined(at, at + 1) && self.punct(at + 1, "=") {
            return Some((if lt { "<=" } else { ">=" }, at + 1));
        }
        Some((if lt { "<" } else { ">" }, at))
    }

    /// Whether a comparison's right side ends at `j`: what follows cannot
    /// continue an arithmetic or method expression.
    fn ends_expression(&self, j: usize) -> bool {
        let Some(t) = self.code.get(j + 1) else {
            return true;
        };
        if t.kind != Kind::Punct {
            return false;
        }
        match self.source.text_of(t) {
            ")" | "]" | "}" | "{" | ";" | "," => true,
            // `&&` and `||` end it; a single `&` or `|` is bitwise.
            p @ ("&" | "|") => self.punct(j + 2, p) && self.joined(j + 1, j + 2),
            // A match guard's `=>`.
            "=" => self.punct(j + 2, ">") && self.joined(j + 1, j + 2),
            _ => false,
        }
    }

    /// Where the condition or expression holding the operation ending at
    /// `last` ends: before the next `{`, `;`, `,` or `=>` at its own depth,
    /// or an unmatched closing delimiter.
    fn context_end(&self, start: usize, last: usize) -> usize {
        // Groups opened between the context's start and `last` stay open.
        let mut depth = 0_usize;
        for k in start..=last {
            if ["(", "[", "{"].iter().any(|p| self.punct(k, p)) {
                depth += 1;
            } else if [")", "]", "}"].iter().any(|p| self.punct(k, p)) {
                depth = depth.saturating_sub(1);
            }
        }
        let mut k = last + 1;
        while let Some(tok) = self.code.get(k) {
            let text = self.source.text_of(tok);
            if tok.kind == Kind::Punct {
                match text {
                    "(" | "[" => depth += 1,
                    "{" if depth > 0 => depth += 1,
                    ")" | "]" | "}" => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    "{" | ";" | "," if depth == 0 => break,
                    "=" if depth == 0 && self.punct(k + 1, ">") && self.joined(k, k + 1) => break,
                    _ => {}
                }
            }
            k += 1;
        }
        k - 1
    }
}

/// The value of an integer literal token, or `None` when out of range.
fn int_value(text: &str) -> Option<u128> {
    let text = text.replace('_', "");
    let (radix, digits) = if let Some(d) = text.strip_prefix("0x") {
        (16, d)
    } else if let Some(d) = text.strip_prefix("0o") {
        (8, d)
    } else if let Some(d) = text.strip_prefix("0b") {
        (2, d)
    } else {
        (10, text.as_str())
    };
    let digits = [
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
    ]
    .iter()
    .find_map(|s| digits.strip_suffix(s))
    .unwrap_or(digits);
    u128::from_str_radix(digits, radix).ok()
}

/// Every sign operation on a recognisable ID in `source`, test code
/// included (the caller exempts it).
pub fn sites(source: &Source) -> Vec<Site> {
    let t = Toks {
        source,
        code: source.code(),
    };
    let mut out = Vec::new();
    for i in 0..t.code.len() {
        let found = [
            t.forward_comparison(i),
            t.reversed_comparison(i),
            t.cast(i),
            t.method(i),
            t.associated(i),
        ];
        for (first, last, shape) in found.into_iter().flatten() {
            let tok = t.code[first];
            out.push(Site {
                line: tok.line,
                offset: tok.start,
                shape,
                context: t.context(first, last),
            });
        }
    }
    out
}

/// A found operation: (first token, last token, normalised shape).
type Found = Option<(usize, usize, String)>;

impl Toks<'_> {
    /// `id <op> N` with the comparison at `i`.
    fn forward_comparison(&self, i: usize) -> Found {
        let (op, op_end) = self.comparison(i)?;
        let name = self.operand_ending(i.checked_sub(1)?)?;
        let (n, n_end) = self.threshold(op_end + 1)?;
        let begin = self.operand_begin(i - 1);
        (self.ends_expression(n_end) && !self.in_arithmetic(begin))
            .then(|| (begin, n_end, format!("{name} {op} {n}")))
    }

    /// `N <op> id` with the comparison at `i`.
    fn reversed_comparison(&self, i: usize) -> Found {
        let (op, op_end) = self.comparison(i)?;
        let (n, n_start) = [i.checked_sub(2), i.checked_sub(1)]
            .into_iter()
            .flatten()
            .find_map(|s| {
                self.threshold(s)
                    .filter(|(_, last)| *last + 1 == i)
                    .map(|(n, _)| (n, s))
            })?;
        if !self.opens_operand(n_start) {
            return None;
        }
        let end = self.operand_start(op_end + 1)?;
        let name = self.operand_ending(end)?;
        self.ends_expression(end)
            .then(|| (n_start, end, format!("{n} {op} {name}")))
    }

    /// `id as uN` with the `as` at `i`.
    fn cast(&self, i: usize) -> Found {
        if !self.source.is_word(&self.code[i], "as") {
            return None;
        }
        let name = self.operand_ending(i.checked_sub(1)?)?;
        let ty = self.name(i + 1).filter(|ty| UNSIGNED.contains(ty))?;
        Some((self.operand_begin(i - 1), i + 1, format!("{name} as {ty}")))
    }

    /// `id.method()`, `id.max(0)`, `id.min(0)`, `id.clamp(0, ..)` with the
    /// `.` at `i`.
    fn method(&self, i: usize) -> Found {
        if !self.punct(i, ".") || !self.punct(i + 2, "(") {
            return None;
        }
        let name = self.operand_ending(i.checked_sub(1)?)?;
        let m = self.name(i + 1)?;
        let close = self.close_of(i + 2)?;
        let begin = self.operand_begin(i - 1);
        if SIGN_METHODS.contains(&m) && close == i + 3 {
            return Some((begin, close, format!("{name}.{m}()")));
        }
        if !matches!(m, "max" | "min" | "clamp") {
            return None;
        }
        let (n, last) = self.threshold(i + 3)?;
        let closes =
            last + 1 == close || (self.punct(last + 1, ",") && (last + 2 == close || m == "clamp"));
        (n == "0" && closes).then(|| (begin, close, format!("{name}.{m}(0)")))
    }

    /// `iN::abs(id)` and kin, `uN::try_from(id)`, with the type at `i`.
    fn associated(&self, i: usize) -> Found {
        let ty = self.name(i)?;
        if !(self.punct(i + 1, ":") && self.punct(i + 2, ":") && self.punct(i + 4, "(")) {
            return None;
        }
        let func = self.name(i + 3)?;
        let assoc = (SIGNED.contains(&ty) && SIGN_METHODS.contains(&func))
            || (UNSIGNED.contains(&ty) && func == "try_from");
        if !assoc {
            return None;
        }
        let end = self.operand_start(i + 5)?;
        let close = if self.punct(end + 1, ",") {
            end + 2
        } else {
            end + 1
        };
        let name = self.operand_ending(end)?;
        self.punct(close, ")")
            .then(|| (i, close, format!("{ty}::{func}({name})")))
    }
}

#[cfg(test)]
mod tests {
    use super::{Entry, e, errors_with, sites};
    use crate::Corpus;
    use crate::lex::Source;

    fn found(src: &str) -> Vec<String> {
        sites(&Source::new(src.to_owned()))
            .into_iter()
            .map(|s| s.shape)
            .collect()
    }

    #[test]
    fn the_fixed_negative_id_drops_are_found() {
        let cases: &[(&str, &[&str])] = &[
            ("fn f() { if wid >= 0 { set.insert(wid); } }", &["wid >= 0"]),
            (
                "fn f() { let v = w.refs().filter(|&r| r >= 0); }",
                &["r >= 0"],
            ),
            ("fn f() { if start < 1 { return Err(e); } }", &["start < 1"]),
            ("fn f() { if nref < 0 { continue; } }", &["nref < 0"]),
            (
                "fn f() { if parent >= 0 { keep(parent); } }",
                &["parent >= 0"],
            ),
            ("fn f() { if node.id() >= 0 { x(); } }", &["id >= 0"]),
            (
                "fn f() { if 0 <= reader.current().id { x(); } }",
                &["0 <= id"],
            ),
            ("fn f() { if 0 <= (id) { x(); } }", &["0 <= id"]),
            ("fn f() { if (id) < 0 { x(); } }", &["id < 0"]),
            ("fn f() { if id > -1 { x(); } }", &["id > -1"]),
            ("fn f() { if id <= -1 { x(); } }", &["id <= -1"]),
            ("fn f() { if id >= 0x0 { x(); } }", &["id >= 0"]),
            ("fn f() { if id >= 0_i64 { x(); } }", &["id >= 0"]),
            ("fn f() { let k = way_id as u64; }", &["way_id as u64"]),
            ("fn f() { let k = (id) as usize; }", &["id as usize"]),
            ("fn f() { let k = id.abs(); }", &["id.abs()"]),
            (
                "fn f() { let k = e.id().unsigned_abs(); }",
                &["id.unsigned_abs()"],
            ),
            ("fn f() { let k = i64::abs(id); }", &["i64::abs(id)"]),
            (
                "fn f() { let k = u64::try_from(id,).ok()?; }",
                &["u64::try_from(id)"],
            ),
            ("fn f() { let k = min_id.max(0); }", &["min_id.max(0)"]),
            ("fn f() { let k = id.clamp(0, 9); }", &["id.clamp(0)"]),
            ("fn f() { let k = 0 < id.abs(); }", &["id.abs()"]),
            (
                "fn f() { let k = id.cast_unsigned(); }",
                &["id.cast_unsigned()"],
            ),
            ("fn f() { v.partition_point(|&id| id <= 0) }", &["id <= 0"]),
            ("fn f() { if -1 < id { x(); } }", &["-1 < id"]),
            ("fn f() { if -1 >= id { x(); } }", &["-1 >= id"]),
            ("fn f() { match 0 < id { _ => 1 } }", &["0 < id"]),
            ("fn f() { match x { _ => 0 < id, } }", &["0 < id"]),
            (
                "fn f() { if element_id(e) < 0 { x(); } }",
                &["element_id < 0"],
            ),
            (
                "fn f() { if 0 < element_id(e) { x(); } }",
                &["0 < element_id"],
            ),
            ("fn f() { let k = id.max(0,); }", &["id.max(0)"]),
            ("fn f() { if a && id < 0 || b { x(); } }", &["id < 0"]),
            (
                "fn f() { match m { MemberId::Node(id) if want_nodes && id >= 0 => emit(id), _ => {} } }",
                &["id >= 0"],
            ),
        ];
        for (src, want) in cases {
            assert_eq!(found(src), *want, "{src}");
        }
    }

    #[test]
    fn other_values_and_tokens_are_not_sign_decisions() {
        for src in [
            "fn f() { if count >= 0 { x(); } }",
            "fn f() { if id >= 2 { x(); } }",
            "fn f() { if id >= 0xff { x(); } }",
            "fn f() { if id >= 0.0 { x(); } }",
            "fn f() { let k = id >> 0; let j = id << 1; }",
            "fn f() -> u64 { 0 }",
            "fn f() { match id { 0 => 1, _ => 0 } }",
            "fn f() { let k = id as i64; }",
            "fn f() { let k = ids.len() as u64; }",
            "fn f() { if id >= 0 + base { x(); } }",
            "fn f() { if id.len() >= 0 { x(); } }",
            "fn f() { let k = id.max(1); }",
            "// if id >= 0 { x(); }",
            "fn f() { let s = \"id >= 0\"; }",
            "fn f() { let k = id_of(x) as u64; }",
            "fn f() { let v: Vec<id> = Vec::new(); }",
            "fn f() { if offset + id < 0 { x(); } }",
            "fn f() { if 0 < id & mask { x(); } }",
            "fn f() { if base - 0 < id { x(); } }",
            "fn f() { let k = mask | id < 0; }",
            "fn f() { let k = mask | 0 < id; }",
        ] {
            assert!(found(src).is_empty(), "{src}: {:?}", found(src));
        }
    }

    const FILE: &str = "crates/lib/src/a.rs";

    fn corpus(lib: &str) -> Corpus {
        Corpus::from_sources([(FILE, lib), ("crates/cli/src/lib.rs", "fn main() {}")])
    }

    fn entry(owner: &'static str, site: &'static str, count: usize) -> Entry {
        e(FILE, owner, site, count, "fixture")
    }

    #[test]
    fn the_inventory_holds_exact_operations_by_owner() {
        let src = "fn route(id: i64) -> bool { id > 0 }";
        assert!(errors_with(&corpus(src), &[entry("route", "id > 0", 1)]).is_empty());
        // Unlisted.
        assert_eq!(errors_with(&corpus(src), &[]).len(), 1);
        // Same count, a different operator: zero moves families.
        let moved = "fn route(id: i64) -> bool { id >= 0 }";
        assert_eq!(
            errors_with(&corpus(moved), &[entry("route", "id > 0", 1)]).len(),
            2
        );
        // A second occurrence in the owner.
        let twice = "fn route(id: i64) -> bool { id > 0 && id > 0 }";
        let held = [entry("route", "id > 0 && id > 0", 1)];
        assert_eq!(errors_with(&corpus(twice), &held).len(), 1);
        // Another owner is not covered.
        let other = "fn select(id: i64) -> bool { id > 0 }";
        assert_eq!(
            errors_with(&corpus(other), &[entry("route", "id > 0", 1)]).len(),
            2
        );
        // A stale entry.
        let none = "fn route() {}";
        assert_eq!(
            errors_with(&corpus(none), &[entry("route", "id > 0", 1)]).len(),
            1
        );
    }

    #[test]
    fn ambiguous_owners_cannot_hold_an_operation() {
        let src = "struct S; impl From<i64> for S { fn from(id: i64) -> S { let _ = id > 0; S } }\n\
                   impl From<i32> for S { fn from(v: i32) -> S { S } }";
        let found = errors_with(&corpus(src), &[entry("S::from", "let _ = id > 0", 1)]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("2 declarations"), "{found:?}");
    }

    #[test]
    fn test_only_code_is_outside_the_rule() {
        for src in [
            "#[cfg(test)] mod tests { fn f(id: i64) -> bool { id < 0 } }",
            "#[test] fn t() { let k = id as u64; }",
            "#![cfg(test)]\nfn f(id: i64) -> bool { id < 0 }",
            "#[cfg(all(test, unix))] fn f(id: i64) -> bool { id < 0 }",
        ] {
            assert!(errors_with(&corpus(src), &[]).is_empty(), "{src}");
        }
        for src in [
            "#[cfg(any(test, feature = \"x\"))] fn f(id: i64) -> bool { id < 0 }",
            "#[cfg(not(test))] fn f(id: i64) -> bool { id < 0 }",
        ] {
            assert_eq!(errors_with(&corpus(src), &[]).len(), 1, "{src}");
        }
    }

    #[test]
    fn the_key_holds_the_receiver_and_the_consumer() {
        let report = "fn push(id: i64) { r.negative |= id < 0; }";
        let held = [entry("push", "r.negative |= id < 0", 1)];
        assert!(errors_with(&corpus(report), &held).is_empty());
        let drop = "fn push(id: i64) { if id < 0 { return; } }";
        assert_eq!(errors_with(&corpus(drop), &held).len(), 2);
        let claim = "fn f() { if claim.min_id < 0 { x(); } }";
        let held = [entry("f", "if claim.min_id < 0", 1)];
        assert!(errors_with(&corpus(claim), &held).is_empty());
        let other = "fn f() { if other.min_id < 0 { x(); } }";
        assert_eq!(errors_with(&corpus(other), &held).len(), 2);
        let early = "fn f() { if sorted && id >= 0 && min_id > id { return; } }";
        let held_early = [entry("f", "if sorted && id >= 0 && min_id > id", 1)];
        assert!(errors_with(&corpus(early), &held_early).is_empty());
        let loose = "fn f() { if sorted && id >= 0 { return; } }";
        assert_eq!(errors_with(&corpus(loose), &held_early).len(), 2);
        let flip = "fn k(id: i64) -> u64 { (id as u64) ^ (1 << 63) }";
        let held_flip = [entry("k", "(id as u64) ^ (1 << 63)", 1)];
        assert!(errors_with(&corpus(flip), &held_flip).is_empty());
        let shifted = "fn k(id: i64) -> u64 { (id as u64) ^ (1 << 62) }";
        assert_eq!(errors_with(&corpus(shifted), &held_flip).len(), 2);
        let negated = "fn n(id: i64) -> bool { !(id < 0) }";
        assert_eq!(
            errors_with(&corpus(negated), &[entry("n", "id < 0", 1)]).len(),
            2
        );
        assert!(errors_with(&corpus(negated), &[entry("n", "!(id < 0)", 1)]).is_empty());
        let blank = [e(FILE, "f", "if claim.min_id < 0", 1, " ")];
        assert_eq!(errors_with(&corpus(claim), &blank).len(), 1);
    }

    #[test]
    fn a_file_mounted_under_a_test_gate_is_scanned_on_its_own() {
        let found = errors_with(
            &Corpus::from_sources([
                ("crates/lib/src/lib.rs", "#[cfg(test)] mod fixture;"),
                (
                    "crates/lib/src/fixture.rs",
                    "fn f(id: i64) -> bool { id < 0 }",
                ),
                ("crates/cli/src/lib.rs", "fn main() {}"),
            ]),
            &[],
        );
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn the_cli_tree_is_audited() {
        let found = errors_with(
            &Corpus::from_sources([
                (FILE, "fn main() {}"),
                (
                    "crates/cli/src/lib.rs",
                    "fn parse(start: i64) -> bool { start < 1 }",
                ),
            ]),
            &[],
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("crates/cli/src/lib.rs:1:"),
            "{found:?}"
        );
    }

    #[test]
    fn a_tree_without_files_fails() {
        let empty = Corpus::from_sources([("crates/other/src/a.rs", "")]);
        assert_eq!(errors_with(&empty, &[]).len(), 2);
    }
}
