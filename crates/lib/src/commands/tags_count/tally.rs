//! One blob's tags, counted through the block validator
//! ([`BlockBuffer::project`]) instead of a built block, and merged into the
//! run's [`CountTable`] by the worker that decoded the blob.
//!
//! The [`TagRuns`] projection records where each selected element's tag
//! arrays lie; nothing is read from them until the blob has been accepted
//! (validation, string table UTF-8, kind claim), so a refused blob counts
//! nothing. The accepted arrays are then read once, as string table index
//! pairs counted per (kind, key index, value index), or per (kind, key
//! index) when the run counts no pair: no string is resolved per tag, no
//! dense node's ID or coordinates are decoded. Each distinct entry is
//! resolved and judged against the [`Selection`] once per blob: a pair the
//! pair expressions admit is hashed for its pair row, and a key the key
//! expressions admit adds its count to the blob's tally of that key, across
//! kinds and values. Only then is anything merged by text, so duplicate
//! string table entries fold into one row.
//!
//! Which tags count is the element walk's rule, the one `TagIter` and
//! `DenseTagIter` apply: an element's tags are its pairs up to the first one
//! whose key or value index lies past the string table (CORRECTNESS.md, "Tag
//! iterators end early on an out-of-range string index"). Counting a key
//! alone still reads and checks its value index. A plain element stops
//! there; a dense node stops counting but its pairs are still read up to its
//! terminator so the next node starts where it should. Index 0 is looked up
//! like any other index, and a dense `0` is a terminator only in key
//! position.

use rustc_hash::FxHashMap;

use super::Selection;
use super::table::{CountTable, key_hash, pair_hash};
use crate::ElementKind;
use crate::error::{Error, Result};
use crate::owned::TypeFilter;
use crate::read::block::{BlockBuffer, GroupProjection, ValidatedBlock};
use crate::read::project_ids::Span;
use crate::read::wire::{
    Cursor, PackedUint32Iter, WireDenseInfo, WireDenseNodes, WireInfo, WireNode, WireRelation,
    WireWay,
};
use crate::tunables::{MAX_RETAINED_SCRATCH_CAPACITY, TAGS_COUNT_ROTATION_STEP as ROTATION_STEP};

/// Where one selected element's, or one dense group's, tags lie.
#[derive(Clone, Copy)]
enum TagRun {
    /// A plain node, way or relation: its `keys` and `vals` arrays.
    Plain {
        kind: ElementKind,
        keys: Span,
        vals: Span,
    },
    /// A dense group's `keys_vals` array: every node's run, in order.
    Dense(Span),
}

/// The projection: the tag arrays of the elements `admits` selects, by
/// position. Element order does not matter to a count, so the validator's
/// order (plain nodes, ways, relations, then the dense message) is kept.
pub(super) struct TagRuns<'s> {
    admits: TypeFilter,
    runs: &'s mut Vec<TagRun>,
}

impl<'s> TagRuns<'s> {
    /// The projection recording into `runs`, which it clears.
    fn new(admits: TypeFilter, runs: &'s mut Vec<TagRun>) -> Self {
        runs.clear();
        TagRuns { admits, runs }
    }

    fn plain(&mut self, kind: ElementKind, keys: &[u8], vals: &[u8]) {
        // Validation holds the two arrays to one length, so an element with
        // no keys has no tags.
        if !keys.is_empty() && self.admits.admits(kind) {
            self.runs.push(TagRun::Plain {
                kind,
                keys: Span::of(keys),
                vals: Span::of(vals),
            });
        }
    }
}

impl GroupProjection for TagRuns<'_> {
    fn node(&mut self, node: &WireNode<'_>, _info: Option<&WireInfo>) {
        self.plain(ElementKind::Node, node.keys_data, node.vals_data);
    }

    fn way(&mut self, way: &WireWay<'_>, _info: Option<&WireInfo>) {
        self.plain(ElementKind::Way, way.keys_data, way.vals_data);
    }

    fn relation(&mut self, relation: &WireRelation<'_>, _info: Option<&WireInfo>) {
        self.plain(
            ElementKind::Relation,
            relation.keys_data,
            relation.vals_data,
        );
    }

    fn dense(
        &mut self,
        dense: &WireDenseNodes<'_>,
        _ids: usize,
        _info: Option<&WireDenseInfo<'_>>,
    ) {
        if !dense.keys_vals_data.is_empty() && self.admits.admits(ElementKind::Node) {
            self.runs
                .push(TagRun::Dense(Span::of(dense.keys_vals_data)));
        }
    }
}

/// One distinct selected row of a blob, waiting to be merged: its shard,
/// hash, string table indices (no value index for a key row) and count.
struct Pending {
    shard: usize,
    hash: u64,
    key: u32,
    value: Option<u32>,
    count: u64,
}

/// A worker's tally scratch, reused across its blobs.
pub(super) struct BlobTally {
    runs: Vec<TagRun>,
    /// Occurrences per (kind, key index, value index), when the run counts
    /// pairs.
    pairs: FxHashMap<(ElementKind, u32, u32), u64>,
    /// Occurrences per (kind, key index), when it counts no pair.
    keys: FxHashMap<(ElementKind, u32), u64>,
    /// The selected key-row occurrences per key index, across kinds and
    /// values.
    key_rows: FxHashMap<u32, u64>,
    pending: Vec<Pending>,
    /// Where the next blob starts walking the shards. Seeded per worker and
    /// stepped per blob, so workers do not all queue on the same shard.
    rotation: usize,
}

impl BlobTally {
    /// A worker's scratch; `seed` sets where its first blob starts.
    pub(super) fn new(seed: usize) -> Self {
        BlobTally {
            runs: Vec::new(),
            pairs: FxHashMap::default(),
            keys: FxHashMap::default(),
            key_rows: FxHashMap::default(),
            pending: Vec::new(),
            rotation: seed.wrapping_mul(ROTATION_STEP),
        }
    }

    /// Count one blob's selected tags into `table`, as `selection` says: a
    /// tag counts in its key row when a key expression whose type prefix
    /// admits the element's kind matches it, and in its pair row when a pair
    /// expression does, each independently.
    pub(super) fn add_blob(
        &mut self,
        buffer: BlockBuffer<'_>,
        admits: TypeFilter,
        selection: &Selection,
        table: &CountTable,
        cancel: &crate::host::Recorder,
    ) -> Result<()> {
        let (TagRuns { .. }, accepted) =
            buffer.project(|_, _| TagRuns::new(admits, &mut self.runs))?;
        self.count(&accepted, selection.counts_pairs())?;
        self.select(&accepted, selection)?;
        self.merge(&accepted, table, cancel)?;
        self.release_outliers();
        Ok(())
    }

    /// The accepted blob's tag occurrences by the element walk's rule
    /// (module docs): per (kind, key index, value index) when `by_pair`,
    /// else per (kind, key index).
    fn count(&mut self, accepted: &ValidatedBlock<'_>, by_pair: bool) -> Result<()> {
        let BlobTally {
            runs, pairs, keys, ..
        } = self;
        pairs.clear();
        keys.clear();
        if by_pair {
            walk_tags(runs, accepted, |kind, k, v| {
                *pairs.entry((kind, k, v)).or_insert(0) += 1;
            })
        } else {
            walk_tags(runs, accepted, |kind, k, _| {
                *keys.entry((kind, k)).or_insert(0) += 1;
            })
        }
    }

    /// The distinct rows the selection admits, each resolved and hashed once,
    /// sorted by shard: every admitted pair, then every key the key
    /// expressions admitted on any kind with any value, with its occurrences
    /// added up.
    fn select(&mut self, accepted: &ValidatedBlock<'_>, selection: &Selection) -> Result<()> {
        let BlobTally {
            pairs,
            keys,
            key_rows,
            pending,
            ..
        } = self;
        pending.clear();
        key_rows.clear();
        for (&(kind, key, value), &count) in pairs.iter() {
            let (k, v) = strings(accepted, key, value)?;
            if selection.admits_pair(kind, k, v) {
                let hash = pair_hash(k.as_bytes(), v.as_bytes());
                pending.push(Pending {
                    shard: CountTable::shard_of(hash),
                    hash,
                    key,
                    value: Some(value),
                    count,
                });
            }
            if selection.admits_key(kind, k) {
                *key_rows.entry(key).or_insert(0) += count;
            }
        }
        for (&(kind, key), &count) in keys.iter() {
            if selection.admits_key(kind, string(accepted, key)?) {
                *key_rows.entry(key).or_insert(0) += count;
            }
        }
        // Two indices holding the same text stay two entries here; the
        // shard adds both to the one row, by bytes.
        for (&key, &count) in key_rows.iter() {
            let hash = key_hash(string(accepted, key)?.as_bytes());
            pending.push(Pending {
                shard: CountTable::shard_of(hash),
                hash,
                key,
                value: None,
                count,
            });
        }
        pending.sort_unstable_by_key(|p| p.shard);
        Ok(())
    }

    /// Merge the pending rows into `table`, one shard locked at a time,
    /// starting at this blob's rotation and wrapping. Cancellation is
    /// checked before each shard.
    fn merge(
        &mut self,
        accepted: &ValidatedBlock<'_>,
        table: &CountTable,
        cancel: &crate::host::Recorder,
    ) -> Result<()> {
        let start = self.rotation % table.shard_count();
        self.rotation = self.rotation.wrapping_add(ROTATION_STEP);
        let split = self.pending.partition_point(|p| p.shard < start);
        let (head, tail) = self.pending.split_at(split);
        let mut held = None;
        for p in tail.iter().chain(head) {
            if held.as_ref().is_none_or(|(index, _)| *index != p.shard) {
                // Release the previous shard before waiting on the next.
                drop(held.take());
                cancel.check()?;
                held = Some((p.shard, table.lock(p.shard)?));
            }
            if let Some((_, shard)) = held.as_mut() {
                match p.value {
                    Some(value) => {
                        let (k, v) = strings(accepted, p.key, value)?;
                        shard.add(p.hash, k, v, p.count)?;
                    }
                    None => shard.add_key(p.hash, string(accepted, p.key)?, p.count)?,
                }
            }
        }
        Ok(())
    }

    /// Drop scratch one outlier blob grew past
    /// `MAX_RETAINED_SCRATCH_CAPACITY`, so a worker does not pin it for the
    /// rest of the run. The table's figure is an estimate (buckets times
    /// entry size).
    fn release_outliers(&mut self) {
        let over = |capacity: usize, size: usize| {
            capacity.saturating_mul(size) > MAX_RETAINED_SCRATCH_CAPACITY
        };
        if over(self.runs.capacity(), size_of::<TagRun>()) {
            self.runs = Vec::new();
        }
        if over(
            self.pairs.capacity(),
            size_of::<((ElementKind, u32, u32), u64)>(),
        ) {
            self.pairs = FxHashMap::default();
        }
        if over(self.keys.capacity(), size_of::<((ElementKind, u32), u64)>()) {
            self.keys = FxHashMap::default();
        }
        if over(self.key_rows.capacity(), size_of::<(u32, u64)>()) {
            self.key_rows = FxHashMap::default();
        }
        if over(self.pending.capacity(), size_of::<Pending>()) {
            self.pending = Vec::new();
        }
    }
}

/// Call `f(kind, key index, value index)` for every tag the element walk
/// counts on the recorded runs (module docs).
fn walk_tags(
    runs: &[TagRun],
    accepted: &ValidatedBlock<'_>,
    mut f: impl FnMut(ElementKind, u32, u32),
) -> Result<()> {
    let bytes = accepted.bytes();
    let strings = accepted.string_count();
    let resolves = |index: u64| {
        u32::try_from(index)
            .ok()
            .filter(|&i| (i as usize) < strings)
    };
    for &run in runs {
        match run {
            TagRun::Plain { kind, keys, vals } => {
                let keys = PackedUint32Iter::new(keys.in_blob(bytes)?);
                let vals = PackedUint32Iter::new(vals.in_blob(bytes)?);
                for (k, v) in keys.zip(vals) {
                    let (Some(k), Some(v)) = (resolves(k.into()), resolves(v.into())) else {
                        break;
                    };
                    f(kind, k, v);
                }
            }
            TagRun::Dense(span) => {
                let mut cursor = Cursor::new(span.in_blob(bytes)?);
                // Whether the current node's tags are still counting.
                let mut live = true;
                while !cursor.is_empty() {
                    let key = cursor.read_varint()?;
                    if key == 0 {
                        live = true;
                        continue;
                    }
                    let value = cursor.read_varint()?;
                    if !live {
                        continue;
                    }
                    match (resolves(key), resolves(value)) {
                        (Some(k), Some(v)) => f(ElementKind::Node, k, v),
                        _ => live = false,
                    }
                }
            }
        }
    }
    Ok(())
}

fn past_the_table() -> Error {
    Error::internal("inspect tags: a counted string index lies past its string table")
}

/// The string of an index already found in the table.
fn string<'b>(accepted: &'b ValidatedBlock<'_>, index: u32) -> Result<&'b str> {
    accepted.string(index as usize).ok_or_else(past_the_table)
}

/// The key and value strings of an index pair already found in the table.
fn strings<'b>(
    accepted: &'b ValidatedBlock<'_>,
    key: u32,
    value: u32,
) -> Result<(&'b str, &'b str)> {
    Ok((string(accepted, key)?, string(accepted, value)?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::Selection;
    use super::{BlobTally, CountTable};
    use crate::owned::TypeFilter;
    use crate::read::wire_fixtures::{
        Group, block, deltas, dense, field_bytes, field_varint, mutations, packed, plain_node,
        projection_fixtures, relation,
    };
    use crate::tag_expr::{Expression, parse_expressions, tag_matches};
    use crate::{Element, ElementKind};

    /// Rows by (key, value), `None` for a key row.
    type Counts = BTreeMap<(String, Option<String>), u64>;

    fn rows(rows: Vec<super::super::TagCount>) -> Counts {
        rows.into_iter()
            .map(|r| ((r.key, r.value), r.count))
            .collect()
    }

    /// The element walk's counts, by osmium's rule written out on its own:
    /// every tag `tags()` yields on an element `admits` selects counts in its
    /// key row when no expression is given or an expression without `=`
    /// matches it, and in its pair row when an expression with `=` matches
    /// it, the two judged independently.
    fn walked(
        block: &crate::PrimitiveBlock,
        admits: TypeFilter,
        expressions: Option<&[Expression]>,
    ) -> Counts {
        let matches = |e: &Expression, kind: ElementKind, k: &str, v: &str| {
            e.type_filter.admits(kind) && tag_matches(&e.matcher, k, v)
        };
        let mut counts = Counts::new();
        for element in block.elements_skip_metadata() {
            let (kind, tags): (ElementKind, Vec<(&str, &str)>) = match &element {
                Element::DenseNode(dn) => (ElementKind::Node, dn.tags().collect()),
                Element::Node(n) => (ElementKind::Node, n.tags().collect()),
                Element::Way(w) => (ElementKind::Way, w.tags().collect()),
                Element::Relation(r) => (ElementKind::Relation, r.tags().collect()),
            };
            if !admits.admits(kind) {
                continue;
            }
            for (k, v) in tags {
                let key_row = expressions.is_none_or(|es| {
                    es.iter()
                        .any(|e| e.matcher.value.is_none() && matches(e, kind, k, v))
                });
                let pair_row = expressions.is_some_and(|es| {
                    es.iter()
                        .any(|e| e.matcher.value.is_some() && matches(e, kind, k, v))
                });
                if key_row {
                    *counts.entry((k.to_owned(), None)).or_insert(0) += 1;
                }
                if pair_row {
                    *counts
                        .entry((k.to_owned(), Some(v.to_owned())))
                        .or_insert(0) += 1;
                }
            }
        }
        counts
    }

    /// Blocks that exercise the tag rules: duplicate string table entries,
    /// index 0 (holding the empty string, and holding text), an index past
    /// the table in the middle of an element's tags, dense value index 0, a
    /// final dense run without its terminator, and tags on every kind.
    fn tag_fixtures() -> Vec<(&'static str, Vec<u8>)> {
        let mut tagged_way = Vec::new();
        field_varint(&mut tagged_way, 1, 71);
        field_bytes(&mut tagged_way, 2, &packed([1, 3, 9, 1]));
        field_bytes(&mut tagged_way, 3, &packed([2, 2, 2, 4]));
        field_bytes(&mut tagged_way, 8, &deltas(&[1, 2]));
        let mut tagged_relation = relation(80, &[(1, 5, 0)]);
        field_bytes(&mut tagged_relation, 2, &packed([4, 1]));
        field_bytes(&mut tagged_relation, 3, &packed([2, 0]));
        vec![
            (
                "duplicate strings, index 0 and an out-of-range index on every kind",
                block(
                    &["", "k", "v", "k", "e"],
                    &[
                        Group {
                            nodes: vec![
                                plain_node(
                                    1,
                                    0,
                                    0,
                                    &[(1, 2), (3, 2), (0, 4), (9, 2), (1, 2)],
                                    None,
                                ),
                                plain_node(2, 0, 0, &[(2, 9), (1, 2)], None),
                            ],
                            dense: Some(dense(
                                &[(3, 0, 0), (4, 0, 0), (5, 0, 0), (6, 0, 0)],
                                &[1, 2, 9, 2, 1, 2, 0, 3, 0, 0, 0, 1, 2, 3, 2],
                            )),
                            ..Group::default()
                        },
                        Group {
                            ways: vec![tagged_way],
                            relations: vec![tagged_relation],
                            ..Group::default()
                        },
                    ],
                ),
            ),
            (
                "index 0 holding text",
                block(
                    &["z", "k", "v"],
                    &[Group {
                        nodes: vec![plain_node(1, 0, 0, &[(0, 0), (1, 2), (0, 2)], None)],
                        dense: Some(dense(&[(2, 0, 0), (3, 0, 0)], &[1, 0, 0, 2, 2])),
                        ..Group::default()
                    }],
                ),
            ),
        ]
    }

    /// For every fixture, every truncation and every probed byte value, under
    /// type filters and expressions: the tally refuses exactly what block
    /// construction refuses, with the same message, and where it accepts, its
    /// counts are the element walk's.
    #[test]
    fn tally_refuses_as_construction_and_counts_as_the_element_walk() {
        let parse = |texts: &[&str]| {
            parse_expressions(&texts.iter().map(|&t| t.to_owned()).collect::<Vec<_>>()).unwrap()
        };
        // Key and pair expressions with type prefixes; overlapping routes
        // with a repeated key expression, a tag both routes count, and a
        // value-only pair matcher; pairs alone.
        let mixed = parse(&["n/k", "w/highway=x", "type", "k=e"]);
        let overlapping = parse(&["k", "k=*", "*=v", "k", "w/e"]);
        let pairs = parse(&["*=*"]);
        let only = |kind| {
            let mut filter = TypeFilter::none();
            match kind {
                ElementKind::Node => filter.nodes = true,
                ElementKind::Way => filter.ways = true,
                ElementKind::Relation => filter.relations = true,
            }
            filter
        };
        let selections: [(TypeFilter, Option<&[Expression]>); 8] = [
            (TypeFilter::all(), None),
            (only(ElementKind::Node), None),
            (only(ElementKind::Way), None),
            (only(ElementKind::Relation), Some(&mixed)),
            (TypeFilter::all(), Some(&mixed)),
            (TypeFilter::all(), Some(&overlapping)),
            (only(ElementKind::Way), Some(&overlapping)),
            (TypeFilter::all(), Some(&pairs)),
        ];
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let mut fixtures = projection_fixtures();
        fixtures.extend(tag_fixtures());
        for (what, bytes) in fixtures {
            assert!(
                crate::PrimitiveBlock::from_vec(bytes.clone()).is_ok(),
                "{what}: fixture must be valid"
            );
            // One worker's scratch across every case, as a worker reuses it.
            let mut tally = BlobTally::new(3);
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let cases =
                std::iter::once(("as built".to_owned(), bytes.clone())).chain(mutations(&bytes));
            for (how, input) in cases {
                let oracle = crate::PrimitiveBlock::from_vec(input.clone());
                for (n, &(admits, exprs)) in selections.iter().enumerate() {
                    let table = CountTable::new();
                    let buffer = crate::read::block::BlockBuffer::new(
                        input.clone(),
                        None,
                        &mut st,
                        &mut gr,
                        None,
                    );
                    let selection = Selection::new(exprs);
                    let counted = tally
                        .add_blob(buffer, admits, &selection, &table, &host.invocation)
                        .and_then(|()| table.into_counts(|_| true, &host.invocation));
                    match (&oracle, counted) {
                        (Err(a), Err(b)) => {
                            assert_eq!(a.to_string(), b.to_string(), "{what}, {how}, {n}");
                        }
                        (Ok(block), Ok((got, _))) => {
                            assert_eq!(
                                rows(got),
                                walked(block, admits, exprs),
                                "{what}, {how}, {n}"
                            );
                        }
                        (oracle, counted) => panic!(
                            "{what}, {how}, {n}: construction accepted {}, the tally {}",
                            oracle.is_ok(),
                            counted.is_ok()
                        ),
                    }
                }
            }
        }
    }

    /// The rules the sweep covers, pinned on one block with the counts
    /// written out, so a walk and a tally that drift together still fail.
    #[test]
    fn tag_rules_on_a_known_block() {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let (_, bytes) = tag_fixtures().swap_remove(0);
        let count = |texts: &[&str]| {
            let parsed =
                parse_expressions(&texts.iter().map(|&t| t.to_owned()).collect::<Vec<_>>())
                    .unwrap();
            let table = CountTable::new();
            let (mut st, mut gr) = (Vec::new(), Vec::new());
            let buffer =
                crate::read::block::BlockBuffer::new(bytes.clone(), None, &mut st, &mut gr, None);
            BlobTally::new(0)
                .add_blob(
                    buffer,
                    TypeFilter::all(),
                    &Selection::new(Some(&parsed)),
                    &table,
                    &host.invocation,
                )
                .unwrap();
            rows(table.into_counts(|_| true, &host.invocation).unwrap().0)
        };
        let key = |k: &str| (k.to_owned(), None);
        let pair = |k: &str, v: &str| (k.to_owned(), Some(v.to_owned()));
        // Plain node 1: k=v twice (indices 1 and 3), ""=e, then an index past
        // the table ends its tags. Plain node 2 ends at once, its key "v" not
        // counted either: the pair's value index is past the table. Dense:
        // node 3 k=v then ends, node 4 k="" (value index 0), node 5 none,
        // node 6 (unterminated) k=v twice. Way: k=v twice, then ends.
        // Relation: e=v and k="".
        let pairs: Counts = [
            (pair("k", "v"), 2 + 1 + 2 + 2),
            (pair("", "e"), 1),
            (pair("k", ""), 1 + 1),
            (pair("e", "v"), 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(count(&["*=*"]), pairs);
        let keys: Counts = [(key("k"), 7 + 2), (key(""), 1), (key("e"), 1)]
            .into_iter()
            .collect();
        // No expression and a bare `*` both count every key.
        assert_eq!(count(&[]), keys);
        assert_eq!(count(&["*"]), keys);
        // A tag both routes select counts in both rows; the repeated key
        // expression counts once.
        let both: Counts = [(key("k"), 9), (pair("k", "v"), 7)].into_iter().collect();
        assert_eq!(count(&["k", "k=v", "k"]), both);
    }

    /// Rows spread over many shards merge to the same totals from any
    /// rotation, and a row seen in two blobs adds up: 600 pairs of one key,
    /// and 600 keys.
    #[test]
    fn counts_add_across_blobs_and_shards() {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let words: Vec<String> = (0..600).map(|i| format!("w{i}")).collect();
        let mut strings: Vec<&str> = vec![""];
        strings.extend(words.iter().map(String::as_str));
        let run = |tags: &[(u64, u64)], selection: &Selection| {
            let bytes = block(
                &strings,
                &[Group {
                    nodes: vec![plain_node(1, 0, 0, tags, None)],
                    ..Group::default()
                }],
            );
            let table = CountTable::new();
            for seed in [0, 5] {
                let (mut st, mut gr) = (Vec::new(), Vec::new());
                let buffer = crate::read::block::BlockBuffer::new(
                    bytes.clone(),
                    None,
                    &mut st,
                    &mut gr,
                    None,
                );
                BlobTally::new(seed)
                    .add_blob(
                        buffer,
                        TypeFilter::all(),
                        selection,
                        &table,
                        &host.invocation,
                    )
                    .unwrap();
            }
            table.into_counts(|_| true, &host.invocation).unwrap()
        };

        let star = parse_expressions(&["*=*".to_owned()]).unwrap();
        let one_key: Vec<(u64, u64)> = (1..=600).map(|i| (1, i)).collect();
        let (got, stats) = run(&one_key, &Selection::new(Some(&star)));
        assert_eq!(got.len(), 600);
        assert_eq!(
            (stats.key_rows, stats.pair_rows, stats.retained_rows),
            (0, 600, 600)
        );
        assert!(got.iter().all(|r| r.key == "w0" && r.count == 2));
        let values: std::collections::BTreeSet<Option<&str>> =
            got.iter().map(|r| r.value.as_deref()).collect();
        assert_eq!(values.len(), 600);
        assert!(!values.contains(&None));

        let many_keys: Vec<(u64, u64)> = (1..=600).map(|i| (i, 0)).collect();
        let (got, stats) = run(&many_keys, &Selection::new(None));
        assert_eq!(
            (stats.key_rows, stats.pair_rows, stats.retained_rows),
            (600, 0, 600)
        );
        assert!(got.iter().all(|r| r.value.is_none() && r.count == 2));
        let keys: std::collections::BTreeSet<&str> = got.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys.len(), 600);
    }
}
