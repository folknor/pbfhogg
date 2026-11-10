//! The one owner of output-header policy.
//!
//! Every PBF-writing command used to assemble its `OSMHeader` by its own rule:
//! some forced the sort flag, some passed it through, extract stamped it
//! unconditionally; raw-copying commands dropped `LocationsOnWays` and the
//! injected prepass declarations from the header while copying the blobs that
//! carry them. A command now states facts about its output ([`OutputFacts`]):
//! how the output is ordered, what it did to way payloads and way blob
//! headers, what bounding box and replication state apply. [`build`] derives
//! every header field from those facts and the input headers by one rule,
//! written out in `docs/cli/commands.md` "Output header".
//!
//! The rule is that a header never claims more than the output contains:
//!
//! - `Sort.Type_then_ID` only when the output is in that order.
//! - `LocationsOnWays`, `pbfhogg.WayMembers-v1`, `pbfhogg.SharedNodePins-v1`
//!   only when every way blob written carries them, with meaning that still
//!   holds for this file.
//! - bbox only when it contains the output's coordinates (or the user asked
//!   for an extract's region envelope with `--set-bounds`).
//! - replication position (timestamp, sequence number) only when the output's
//!   content is drawn from that position, and the feed (base URL) only while
//!   the content stays in that feed's ID space; see [`Replication`].
//! - optional features the crate does not know are never carried: nothing can
//!   vouch for them after a rewrite. Only [`verbatim`] (degrade's header-only
//!   passthrough) forwards a header field for field.
//!
//! `--generator` / `--output-header` are applied last, and replace only the
//! field they name; an empty `--output-header` value omits that field.

use crate::HeaderBlock;
use crate::block_builder::HeaderBuilder;
use crate::read::block::Feature;
use crate::read::wire_schema::HeaderBlockField;

use super::{FieldOverride, HeaderOverrides, Result};
use crate::error::Error;

/// How the output's element order relates to `Sort.Type_then_ID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    /// The command emits type-then-ID order by construction (sort, renumber,
    /// apply-changes, `cat --dedupe`).
    Sorted,
    /// The output keeps the input's relative order: sorted exactly when there
    /// is one input and it declares `Sort.Type_then_ID`.
    AsInput,
    /// The command breaks the order on purpose (`degrade --unsort*`).
    Unsorted,
}

/// What the command does to way payloads and way blob headers. The three way
/// enrichments are derived from this:
///
/// | variant | `LocationsOnWays` | `SharedNodePins-v1` | `WayMembers-v1` |
/// |---|---|---|---|
/// | `Reencoded`, `Stripped` | no | no | no |
/// | `ReencodedWithLocations` | if every input | no | no |
/// | `PayloadsCopied` | if every input | if the one input | no |
/// | `BlobsCopied` | if every input | if the one input | if the one input |
/// | `Located` | yes | if `prepass` | if `prepass` |
///
/// The two prepass features are global facts about a file (relation
/// membership, a node shared by two or more way positions), so the copied
/// variants carry them only from a single input whose ways and relations reach
/// the output with unchanged content, all of them or a subset. A subset only
/// over-approximates (a membership superset, extra pins), which ADR-0007
/// allows. Combining inputs can add a relation or a sharing obligation the
/// copied bitmaps never saw, so a multi-input output never declares them.
/// `LocationsOnWays` is a per-way payload guarantee and survives combining:
/// each way carries the coordinates its own input recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ways {
    /// Ways are decoded and re-encoded without inline coordinates, field 20
    /// or field 5. Nothing is declared; a declared input feature is warned.
    Reencoded,
    /// [`Ways::Reencoded`] where the user asked for the loss
    /// (`degrade --strip-locations`), so it is not warned.
    Stripped,
    /// Ways are re-encoded with their inline coordinates (repack,
    /// `apply-changes -l`); the prepass fields are not written.
    ReencodedWithLocations,
    /// Every way payload is copied byte for byte, but some way blob headers
    /// are rebuilt (indexdata added to an unindexed blob), which drops field 5.
    PayloadsCopied,
    /// Every way blob is copied whole: payload and `BlobHeader`.
    BlobsCopied,
    /// add-locations-to-ways: every way gets inline coordinates, and the
    /// prepass fields when injected.
    Located { prepass: bool },
}

impl Ways {
    /// The variant for a blob-copying command that knows, before it writes
    /// the header, whether any way blob is decoded and re-encoded and whether
    /// any is reframed.
    pub(crate) fn from_copy(any_way_rewritten: bool, any_way_reframed: bool) -> Self {
        if any_way_rewritten {
            Self::Reencoded
        } else if any_way_reframed {
            Self::PayloadsCopied
        } else {
            Self::BlobsCopied
        }
    }
}

/// The output's bounding box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Bbox {
    /// The output's coordinates come from the inputs' (a subset, or the
    /// same): the union of the input boxes when every input declares one,
    /// widened onto the decimicrodegree grid a re-encoded node lands on.
    Carry,
    /// `extract --set-bounds`: the region envelope (left, bottom, right, top)
    /// the user asked for.
    Region(f64, f64, f64, f64),
    /// No box: the command can move coordinates outside the input's box
    /// (apply-changes) or was asked to strip it.
    Drop,
}

/// The output's replication state.
///
/// The replication fields name the feed (base URL) and the position in it
/// (timestamp, sequence number) the output's content was drawn from: every
/// object is in that feed's ID space, and a snapshot's objects are at their
/// version as of that position. They do not promise that applying the feed's
/// later diffs maintains a selection: an extract or a filter grows under them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replication {
    /// The output's content is drawn from the inputs' replication state (all
    /// of it, a subset, or a re-encoding): kept when every input carries the
    /// same timestamp, sequence number and base URL.
    Carry,
    /// time-filter: the output holds the data as of `cutoff` (UNIX seconds).
    /// The timestamp becomes `min(input, cutoff)`; the sequence number is kept
    /// only when the input's timestamp is known and not after the cutoff.
    AsOf(i64),
    /// apply-changes: the output has moved past the input's position to one
    /// the command cannot name (an OSC carries no sequence number, nor proof
    /// that it continues the input's feed without a gap). The timestamp and
    /// sequence number are dropped; the base URL, which names the feed
    /// rather than a position in it, is kept as provenance. A caller that
    /// knows the new state stamps it with `--output-header`.
    FeedOnly,
    /// renumber: the content left the feed's ID space, so the feed's diffs
    /// address other objects than the ones in the file. Nothing is kept.
    Drop,
}

/// The facts a command states about its output.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputFacts {
    pub(crate) order: Order,
    pub(crate) ways: Ways,
    pub(crate) bbox: Bbox,
    pub(crate) replication: Replication,
}

impl OutputFacts {
    /// Input order, input bbox and replication state, with `ways`.
    pub(crate) fn carry(ways: Ways) -> Self {
        Self {
            order: Order::AsInput,
            ways,
            bbox: Bbox::Carry,
            replication: Replication::Carry,
        }
    }

    /// [`carry`](Self::carry) for a command whose output is sorted by
    /// construction.
    pub(crate) fn sorted(ways: Ways) -> Self {
        Self {
            order: Order::Sorted,
            ..Self::carry(ways)
        }
    }
}

/// The optional features an output header declares. Commands that can only
/// discover mid-run that a blob would break a declaration consult this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Declared {
    pub(crate) sorted: bool,
    pub(crate) locations_on_ways: bool,
    pub(crate) way_members: bool,
    pub(crate) shared_node_pins: bool,
}

impl Declared {
    /// Any of the three way enrichments.
    pub(crate) fn way_enrichment(self) -> bool {
        self.locations_on_ways || self.way_members || self.shared_node_pins
    }

    /// The error for a blob that would break a declaration already written.
    ///
    /// Commands that copy blobs raw decide their header from a first-blob
    /// indexdata probe. A later way blob without indexdata goes through a
    /// branch that rebuilds its header or re-encodes it, which would leave a
    /// way blob without the enrichment the header promised. Refusing is
    /// conservative (the re-encoded blob might not have needed it), but a
    /// successful output never lies.
    pub(crate) fn broken_by_unindexed_way_blob(self, what: &str) -> Error {
        let features = [
            (self.locations_on_ways, HeaderBlock::LOCATIONS_ON_WAYS),
            (self.way_members, HeaderBlock::WAY_MEMBERS_V1),
            (self.shared_node_pins, HeaderBlock::SHARED_NODE_PINS_V1),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(", ");
        Error::invalid_data(format!(
            "a way blob without indexdata follows an indexed first blob, and {what} \
             would drop {features} from it after the output header already declared \
             them. Give every blob of the input indexdata, or none, or strip \
             the enrichment from it, and rerun"
        ))
    }
}

/// A built output header: the `HeaderBlock` bytes and what they declare.
pub(crate) struct OutputHeader {
    pub(crate) bytes: Vec<u8>,
    pub(crate) declared: Declared,
}

/// Build the output header for a command over `inputs` (one header per input
/// file, in input order) from the facts it states. Warns, once, when an input
/// declares a way enrichment the output does not.
pub(crate) fn build(
    inputs: &[HeaderBlock],
    facts: &OutputFacts,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<OutputHeader> {
    let Some(first) = inputs.first() else {
        return Err(Error::invalid_argument(
            "no input headers to build an output header from (no input files specified)",
        ));
    };
    let single = inputs.len() == 1;
    let all = |has: fn(&HeaderBlock) -> bool| inputs.iter().all(has);
    let one = |has: fn(&HeaderBlock) -> bool| single && has(first);

    let declared = Declared {
        sorted: match facts.order {
            Order::Sorted => true,
            Order::AsInput => one(HeaderBlock::is_sorted),
            Order::Unsorted => false,
        },
        locations_on_ways: match facts.ways {
            Ways::Reencoded | Ways::Stripped => false,
            Ways::ReencodedWithLocations | Ways::PayloadsCopied | Ways::BlobsCopied => {
                all(HeaderBlock::has_locations_on_ways)
            }
            Ways::Located { .. } => true,
        },
        shared_node_pins: match facts.ways {
            Ways::Reencoded | Ways::Stripped | Ways::ReencodedWithLocations => false,
            Ways::PayloadsCopied | Ways::BlobsCopied => one(HeaderBlock::has_shared_node_pins_v1),
            Ways::Located { prepass } => prepass,
        },
        way_members: match facts.ways {
            Ways::BlobsCopied => one(HeaderBlock::has_way_members_v1),
            Ways::Located { prepass } => prepass,
            _ => false,
        },
    };

    if facts.ways != Ways::Stripped {
        warn_undeclared(inputs, declared, host);
    }

    let mut hb = HeaderBuilder::new();
    match facts.bbox {
        Bbox::Carry => {
            if let Some((left, bottom, right, top)) = carried_bbox(inputs) {
                hb = hb.input_bbox(left, bottom, right, top);
            }
        }
        Bbox::Region(left, bottom, right, top) => hb = hb.bbox(left, bottom, right, top),
        Bbox::Drop => {}
    }

    let (mut ts, mut seq, mut url) = agreed_replication(inputs);
    match facts.replication {
        Replication::Carry => {}
        Replication::AsOf(cutoff) => match ts {
            Some(t) if t <= cutoff => {}
            Some(_) => {
                ts = Some(cutoff);
                seq = None;
            }
            // Without a timestamp nothing proves the filter left the input's
            // replication state intact, so its sequence number cannot stay.
            None => seq = None,
        },
        Replication::FeedOnly => (ts, seq) = (None, None),
        Replication::Drop => (ts, seq, url) = (None, None, None),
    }
    if let Some(ts) = ts {
        hb = hb.replication_timestamp(ts);
    }
    if let Some(seq) = seq {
        hb = hb.replication_sequence_number(seq);
    }
    if let Some(url) = url {
        hb = hb.replication_base_url(url);
    }
    if let Some(source) = agreed(inputs, HeaderBlock::source) {
        hb = hb.source(source);
    }
    if inputs.iter().any(HeaderBlock::has_historical_information) {
        hb = hb.historical();
    }

    if declared.sorted {
        hb = hb.sorted();
    }
    if declared.locations_on_ways {
        hb = hb.optional_feature(Feature::LOCATIONS_ON_WAYS);
    }
    if declared.way_members {
        hb = hb.optional_feature(Feature::WAY_MEMBERS_V1);
    }
    if declared.shared_node_pins {
        hb = hb.optional_feature(Feature::SHARED_NODE_PINS_V1);
    }

    let bytes = overrides.apply(hb).build()?;
    Ok(OutputHeader { bytes, declared })
}

/// [`build`] for a command with one input.
pub(crate) fn build_one(
    input: &HeaderBlock,
    facts: &OutputFacts,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<OutputHeader> {
    build(std::slice::from_ref(input), facts, overrides, host)
}

/// degrade's header-only passthrough: forward the input `HeaderBlock` field
/// for field, so the writing program, `source`, every optional feature
/// (custom ones included) and unknown fields survive. `--strip-bbox` removes
/// the bbox; each override replaces exactly the field it names, every
/// occurrence of it, and leaves the rest alone. A cleared field is stripped
/// and nothing is written in its place.
pub(crate) fn verbatim(
    header_block: &[u8],
    strip_bbox: bool,
    overrides: &HeaderOverrides,
) -> Result<Vec<u8>> {
    use crate::read::wire_schema::header_block as field;
    use protohoggr::{encode_bytes_field_always, encode_int64_field_always};

    let mut strip = Vec::with_capacity(5);
    if strip_bbox {
        strip.push(HeaderBlockField::Bbox);
    }
    if overrides.generator.is_some() {
        strip.push(HeaderBlockField::WritingProgram);
    }
    if overrides.replication_timestamp != FieldOverride::Unchanged {
        strip.push(HeaderBlockField::ReplicationTimestamp);
    }
    if overrides.replication_sequence_number != FieldOverride::Unchanged {
        strip.push(HeaderBlockField::ReplicationSequenceNumber);
    }
    if overrides.replication_base_url != FieldOverride::Unchanged {
        strip.push(HeaderBlockField::ReplicationBaseUrl);
    }
    if strip.is_empty() {
        return Ok(header_block.to_vec());
    }
    let mut out = Vec::with_capacity(header_block.len());
    crate::writer::strip_header_block_fields(header_block, &strip, &mut out)?;
    if let Some(program) = &overrides.generator {
        encode_bytes_field_always(&mut out, field::WRITING_PROGRAM, program.as_bytes());
    }
    if let FieldOverride::Set(ts) = overrides.replication_timestamp {
        encode_int64_field_always(&mut out, field::REPLICATION_TIMESTAMP, ts);
    }
    if let FieldOverride::Set(seq) = overrides.replication_sequence_number {
        encode_int64_field_always(&mut out, field::REPLICATION_SEQUENCE_NUMBER, seq);
    }
    if let FieldOverride::Set(url) = &overrides.replication_base_url {
        encode_bytes_field_always(&mut out, field::REPLICATION_BASE_URL, url.as_bytes());
    }
    Ok(out)
}

/// Warn once when an input declares a way enrichment the output does not.
/// The text says the declaration is gone, not the data: a multi-input raw
/// `cat` keeps every byte of the prepass fields and still cannot vouch for
/// them, and an output mixing copied and re-encoded ways keeps some inline
/// coordinates without declaring them.
fn warn_undeclared(inputs: &[HeaderBlock], declared: Declared, host: &crate::host::Entered<'_>) {
    let dropped: Vec<&str> = [
        (
            declared.locations_on_ways,
            HeaderBlock::has_locations_on_ways as fn(&HeaderBlock) -> bool,
            HeaderBlock::LOCATIONS_ON_WAYS,
        ),
        (
            declared.way_members,
            HeaderBlock::has_way_members_v1,
            HeaderBlock::WAY_MEMBERS_V1,
        ),
        (
            declared.shared_node_pins,
            HeaderBlock::has_shared_node_pins_v1,
            HeaderBlock::SHARED_NODE_PINS_V1,
        ),
    ]
    .into_iter()
    .filter(|(kept, has, _)| !kept && inputs.iter().any(has))
    .map(|(_, _, name)| name)
    .collect();
    if dropped.is_empty() {
        return;
    }
    let list = dropped.join(", ");
    crate::host::diag!(
        host.diagnostics,
        Warning,
        "header.way_metadata_loss",
        "the input declares {list}, which the output does not declare: this command \
         does not keep it on every way it writes, or cannot vouch for it in the output"
    );
}

/// The bbox every input declares, as their union, widened so it still
/// contains a node after re-encoding: a decoded coordinate is truncated toward
/// zero onto the 100-nanodegree (decimicrodegree) grid, which can move an
/// off-grid node on the box edge just outside the box. Each edge becomes the
/// outer of itself and its truncation. A box already on the grid (every
/// box pbfhogg or osmium writes from degrees) is unchanged. `None` when any
/// input has no box: its extent is unknown, so the union is too.
fn carried_bbox(inputs: &[HeaderBlock]) -> Option<(f64, f64, f64, f64)> {
    let boxes: Vec<_> = inputs
        .iter()
        .map(HeaderBlock::bbox)
        .collect::<Option<Vec<_>>>()?;
    let fold = |pick: fn(f64, f64) -> f64, get: fn(&crate::HeaderBBox) -> f64| {
        boxes.iter().map(get).reduce(pick).unwrap_or(f64::NAN)
    };
    let (left, bottom, right, top) = (
        fold(f64::min, |b| b.left),
        fold(f64::min, |b| b.bottom),
        fold(f64::max, |b| b.right),
        fold(f64::max, |b| b.top),
    );
    Some((
        widen(left, i64::min),
        widen(bottom, i64::min),
        widen(right, i64::max),
        widen(top, i64::max),
    ))
}

/// One bbox edge, in degrees, widened to cover its own truncation onto the
/// decimicrodegree grid.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn widen(edge: f64, outer: fn(i64, i64) -> i64) -> f64 {
    // Header boxes are nanodegree integers; this recovers the integer exactly
    // (|n| <= 1.8e11, well inside f64's exact range).
    let nanos = (edge * 1e9).round() as i64;
    // Integer division truncates toward zero, as the decoder's does.
    let truncated = nanos / 100 * 100;
    outer(nanos, truncated) as f64 / 1e9
}

/// The replication state every input agrees on. The base URL names the feed
/// and is kept when every input names the same one. The timestamp and
/// sequence number name a position in that feed, so they are kept only when
/// every input carries the same timestamp, sequence number *and* base URL:
/// sequence 100 of two different feeds (or of inputs that do not name their
/// feed alike) is not one replication state.
fn agreed_replication(inputs: &[HeaderBlock]) -> (Option<i64>, Option<i64>, Option<&str>) {
    fn state(h: &HeaderBlock) -> (Option<i64>, Option<i64>, Option<&str>) {
        (
            h.osmosis_replication_timestamp(),
            h.osmosis_replication_sequence_number(),
            h.osmosis_replication_base_url(),
        )
    }
    let url = agreed(inputs, HeaderBlock::osmosis_replication_base_url);
    let first = state(&inputs[0]);
    if inputs.iter().all(|h| state(h) == first) {
        (first.0, first.1, url)
    } else {
        (None, None, url)
    }
}

/// The value every header agrees on for one optional field, or `None` when
/// any header lacks it or they disagree. `Some("")` is a real agreed value,
/// distinct from absence. Callers guarantee `headers` is non-empty.
fn agreed<'a, T: PartialEq + Copy>(
    headers: &'a [HeaderBlock],
    get: impl Fn(&'a HeaderBlock) -> Option<T>,
) -> Option<T> {
    let first = get(&headers[0])?;
    headers
        .iter()
        .all(|h| get(h) == Some(first))
        .then_some(first)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn header(hb: HeaderBuilder<'_>) -> HeaderBlock {
        HeaderBlock::parse_from_bytes(&hb.build().unwrap()).unwrap()
    }

    fn built(inputs: &[HeaderBlock], facts: &OutputFacts) -> HeaderBlock {
        let host = crate::host::Host::entered_for_tests();
        let out = build(inputs, facts, &HeaderOverrides::default(), &host).unwrap();
        HeaderBlock::parse_from_bytes(&out.bytes).unwrap()
    }

    fn enriched() -> HeaderBlock {
        header(
            HeaderBuilder::new()
                .sorted()
                .optional_feature(Feature::LOCATIONS_ON_WAYS)
                .optional_feature(Feature::WAY_MEMBERS_V1)
                .optional_feature(Feature::SHARED_NODE_PINS_V1),
        )
    }

    /// The feature table in the `Ways` doc, row by row.
    #[test]
    fn way_enrichment_follows_the_table() {
        let one = [enriched()];
        let two = [enriched(), enriched()];
        let check = |inputs: &[HeaderBlock], ways, want: (bool, bool, bool)| {
            let h = built(inputs, &OutputFacts::carry(ways));
            assert_eq!(
                (
                    h.has_locations_on_ways(),
                    h.has_shared_node_pins_v1(),
                    h.has_way_members_v1()
                ),
                want,
                "{ways:?} over {} input(s)",
                inputs.len()
            );
        };
        check(&one, Ways::Reencoded, (false, false, false));
        check(&one, Ways::Stripped, (false, false, false));
        check(&one, Ways::ReencodedWithLocations, (true, false, false));
        check(&one, Ways::PayloadsCopied, (true, true, false));
        check(&one, Ways::BlobsCopied, (true, true, true));
        check(&two, Ways::BlobsCopied, (true, false, false));
        check(&two, Ways::PayloadsCopied, (true, false, false));
        let plain = [header(HeaderBuilder::new())];
        check(
            &plain,
            Ways::Located { prepass: false },
            (true, false, false),
        );
        check(&plain, Ways::Located { prepass: true }, (true, true, true));
        // One input without LocationsOnWays withholds it from the combination.
        check(
            &[enriched(), header(HeaderBuilder::new())],
            Ways::BlobsCopied,
            (false, false, false),
        );
    }

    #[test]
    fn order_as_input_is_sorted_only_for_one_sorted_input() {
        let sorted = header(HeaderBuilder::new().sorted());
        let unsorted = header(HeaderBuilder::new());
        let facts = OutputFacts::carry(Ways::Reencoded);
        assert!(built(std::slice::from_ref(&sorted), &facts).is_sorted());
        assert!(!built(std::slice::from_ref(&unsorted), &facts).is_sorted());
        assert!(!built(&[sorted.clone(), sorted.clone()], &facts).is_sorted());
        assert!(
            built(
                std::slice::from_ref(&unsorted),
                &OutputFacts::sorted(Ways::Reencoded)
            )
            .is_sorted()
        );
        let unsort = OutputFacts {
            order: Order::Unsorted,
            ..facts
        };
        assert!(!built(&[sorted], &unsort).is_sorted());
    }

    /// An off-grid edge widens to cover the truncated coordinate a re-encode
    /// produces; an on-grid edge is unchanged.
    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn carried_bbox_covers_the_decimicro_grid() {
        let input = header(HeaderBuilder::new().bbox(1.000_000_04, -1.000_000_06, 2.0, 3.0));
        let b = built(&[input], &OutputFacts::carry(Ways::Reencoded))
            .bbox()
            .unwrap();
        assert_eq!((b.left * 1e9).round() as i64, 1_000_000_000);
        assert_eq!((b.bottom * 1e9).round() as i64, -1_000_000_060);
        assert_eq!((b.right * 1e9).round() as i64, 2_000_000_000);
        assert_eq!((b.top * 1e9).round() as i64, 3_000_000_000);
    }

    #[test]
    fn replication_state_is_agreed_as_a_unit() {
        let a = header(
            HeaderBuilder::new()
                .replication_sequence_number(100)
                .replication_base_url("https://a/"),
        );
        let b = header(
            HeaderBuilder::new()
                .replication_sequence_number(100)
                .replication_base_url("https://b/"),
        );
        let h = built(&[a.clone(), b], &OutputFacts::carry(Ways::Reencoded));
        assert_eq!(h.osmosis_replication_sequence_number(), None);
        assert_eq!(h.osmosis_replication_base_url(), None);
        let h = built(&[a.clone(), a], &OutputFacts::carry(Ways::Reencoded));
        assert_eq!(h.osmosis_replication_sequence_number(), Some(100));
    }

    #[test]
    fn as_of_lowers_the_timestamp_and_drops_an_unproven_sequence() {
        let facts = |cutoff| OutputFacts {
            replication: Replication::AsOf(cutoff),
            ..OutputFacts::carry(Ways::Reencoded)
        };
        let input = header(
            HeaderBuilder::new()
                .replication_timestamp(1000)
                .replication_sequence_number(7),
        );
        let later = built(std::slice::from_ref(&input), &facts(2000));
        assert_eq!(later.osmosis_replication_timestamp(), Some(1000));
        assert_eq!(later.osmosis_replication_sequence_number(), Some(7));
        let earlier = built(std::slice::from_ref(&input), &facts(500));
        assert_eq!(earlier.osmosis_replication_timestamp(), Some(500));
        assert_eq!(earlier.osmosis_replication_sequence_number(), None);
        let no_ts = header(HeaderBuilder::new().replication_sequence_number(7));
        let h = built(&[no_ts], &facts(500));
        assert_eq!(h.osmosis_replication_timestamp(), None);
        assert_eq!(h.osmosis_replication_sequence_number(), None);
    }

    /// FeedOnly keeps the feed and drops the position; Drop keeps neither.
    /// An override still applies last.
    #[test]
    fn feed_only_and_drop_shed_the_position() {
        let input = header(
            HeaderBuilder::new()
                .replication_timestamp(1000)
                .replication_sequence_number(7)
                .replication_base_url("https://feed.invalid/"),
        );
        let with = |replication| OutputFacts {
            replication,
            ..OutputFacts::carry(Ways::Reencoded)
        };
        let state = |h: HeaderBlock| {
            (
                h.osmosis_replication_timestamp(),
                h.osmosis_replication_sequence_number(),
                h.osmosis_replication_base_url().map(str::to_owned),
            )
        };
        let feed_only = built(std::slice::from_ref(&input), &with(Replication::FeedOnly));
        assert_eq!(
            state(feed_only),
            (None, None, Some("https://feed.invalid/".to_owned()))
        );
        let dropped = built(std::slice::from_ref(&input), &with(Replication::Drop));
        assert_eq!(state(dropped), (None, None, None));

        let host = crate::host::Host::entered_for_tests();
        let stamped = HeaderOverrides {
            replication_sequence_number: FieldOverride::Set(8),
            ..Default::default()
        };
        let out = build(
            std::slice::from_ref(&input),
            &with(Replication::FeedOnly),
            &stamped,
            &host,
        )
        .unwrap();
        let h = HeaderBlock::parse_from_bytes(&out.bytes).unwrap();
        assert_eq!(h.osmosis_replication_sequence_number(), Some(8));
    }

    /// A cleared override removes the field the policy would have written,
    /// and leaves the other fields alone.
    #[test]
    fn a_cleared_override_omits_its_field() {
        let input = header(
            HeaderBuilder::new()
                .replication_timestamp(0)
                .replication_sequence_number(0)
                .replication_base_url(""),
        );
        let host = crate::host::Host::entered_for_tests();
        let cleared = HeaderOverrides::parse(
            None,
            &[
                "osmosis_replication_sequence_number=5".to_owned(),
                "osmosis_replication_sequence_number=".to_owned(),
                "osmosis_replication_base_url=".to_owned(),
            ],
        )
        .unwrap();
        let out = build(
            std::slice::from_ref(&input),
            &OutputFacts::carry(Ways::Reencoded),
            &cleared,
            &host,
        )
        .unwrap();
        let h = HeaderBlock::parse_from_bytes(&out.bytes).unwrap();
        assert_eq!(h.osmosis_replication_timestamp(), Some(0));
        assert_eq!(h.osmosis_replication_sequence_number(), None);
        assert_eq!(h.osmosis_replication_base_url(), None);
    }

    #[test]
    fn source_survives_only_when_every_input_agrees() {
        let with = |s: Option<&str>| {
            let mut hb = HeaderBuilder::new();
            if let Some(s) = s {
                hb = hb.source(s);
            }
            header(hb)
        };
        let facts = OutputFacts::carry(Ways::Reencoded);
        assert_eq!(
            built(&[with(Some("a")), with(Some("a"))], &facts).source(),
            Some("a")
        );
        for disagree in [
            [with(Some("a")), with(Some("b"))],
            [with(Some("a")), with(None)],
        ] {
            assert_eq!(built(&disagree, &facts).source(), None);
        }
    }

    /// A single input's fields all reach the output: bbox, replication state
    /// (zero values included), source, history.
    #[test]
    fn a_single_input_is_carried_whole() {
        let input = header(
            HeaderBuilder::new()
                .bbox(1.0, 10.0, 2.0, 20.0)
                .replication_timestamp(0)
                .replication_sequence_number(0)
                .replication_base_url("https://feed.invalid/")
                .source("geofabrik.de")
                .historical()
                .writing_program("other-tool"),
        );
        let h = built(&[input], &OutputFacts::carry(Ways::Reencoded));
        let b = h.bbox().unwrap();
        assert_eq!((b.left, b.bottom, b.right, b.top), (1.0, 10.0, 2.0, 20.0));
        assert_eq!(h.osmosis_replication_timestamp(), Some(0));
        assert_eq!(h.osmosis_replication_sequence_number(), Some(0));
        assert_eq!(
            h.osmosis_replication_base_url(),
            Some("https://feed.invalid/")
        );
        assert_eq!(h.source(), Some("geofabrik.de"));
        assert!(h.has_historical_information());
        assert_eq!(
            h.writing_program(),
            Some(concat!("pbfhogg/", env!("CARGO_PKG_VERSION")))
        );
    }

    /// Empty `source` and base URL survive the policy as present values.
    #[test]
    fn empty_source_and_base_url_are_carried() {
        let input = header(HeaderBuilder::new().source("").replication_base_url(""));
        let h = built(&[input], &OutputFacts::carry(Ways::Reencoded));
        assert_eq!(h.source(), Some(""));
        assert_eq!(h.osmosis_replication_base_url(), Some(""));
    }

    /// An empty input slice is an error, not a panic and not a NaN bbox.
    #[test]
    fn an_empty_input_slice_is_refused() {
        let host = crate::host::Host::entered_for_tests();
        assert!(
            build(
                &[],
                &OutputFacts::carry(Ways::Reencoded),
                &HeaderOverrides::default(),
                &host
            )
            .is_err()
        );
    }

    /// An override replaces exactly its own field; everything else in a
    /// verbatim header survives, custom features included.
    #[test]
    fn verbatim_override_replaces_only_its_field() {
        let input = HeaderBuilder::new()
            .writing_program("osmium/1.16")
            .replication_timestamp(1000)
            .replication_sequence_number(7)
            .optional_feature("x-custom")
            .optional_feature(Feature::WAY_MEMBERS_V1)
            .build()
            .unwrap();
        let overrides = HeaderOverrides {
            generator: Some("mine".into()),
            ..Default::default()
        };
        let h =
            HeaderBlock::parse_from_bytes(&verbatim(&input, false, &overrides).unwrap()).unwrap();
        assert_eq!(h.writing_program(), Some("mine"));
        assert_eq!(h.osmosis_replication_timestamp(), Some(1000));
        assert_eq!(h.osmosis_replication_sequence_number(), Some(7));
        assert!(h.optional_features().iter().any(|f| f == "x-custom"));
        assert!(h.has_way_members_v1());
        assert_eq!(
            verbatim(&input, false, &HeaderOverrides::default()).unwrap(),
            input
        );

        // A clear strips the field and writes nothing in its place.
        let cleared = HeaderOverrides {
            replication_timestamp: FieldOverride::Clear,
            ..Default::default()
        };
        let h = HeaderBlock::parse_from_bytes(&verbatim(&input, false, &cleared).unwrap()).unwrap();
        assert_eq!(h.osmosis_replication_timestamp(), None);
        assert_eq!(h.osmosis_replication_sequence_number(), Some(7));
        assert_eq!(h.writing_program(), Some("osmium/1.16"));
    }
}
