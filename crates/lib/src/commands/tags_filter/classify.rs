//! The dependency pass selects IDs and reads refs directly from the wire
//! fields recorded by block validation.

use super::masks::{BlockMasks, MaskScratch, TagPredicates};
use crate::ElementKind;
use crate::error::{Error, Result};
use crate::read::block::{BlockBuffer, GroupProjection};
use crate::read::elements::WayRefIter;
use crate::read::project_ids::Span;
use crate::read::wire::{
    Cursor, PackedUint32Iter, WireDenseInfo, WireDenseNodes, WireInfo, WireNode, WireRelation,
    WireWay,
};

#[derive(Clone, Copy)]
enum Run {
    Plain {
        kind: ElementKind,
        id: i64,
        keys: Span,
        vals: Span,
        refs: Span,
    },
    Dense {
        ids: Span,
        tags: Span,
    },
}

struct Runs<'s> {
    runs: &'s mut Vec<Run>,
    kinds: [bool; 3],
    invert: bool,
}

impl Runs<'_> {
    fn plain(&mut self, kind: ElementKind, id: i64, keys: &[u8], vals: &[u8], refs: &[u8]) {
        let applies = self.kinds[kind.ordinal() as usize];
        if self.invert || (applies && !keys.is_empty()) {
            let (keys, vals) = if applies {
                (keys, vals)
            } else {
                (&[][..], &[][..])
            };
            self.runs.push(Run::Plain {
                kind,
                id,
                keys: Span::of(keys),
                vals: Span::of(vals),
                refs: Span::of(refs),
            });
        }
    }
}

impl GroupProjection for Runs<'_> {
    fn node(&mut self, node: &WireNode<'_>, _info: Option<&WireInfo>) {
        self.plain(
            ElementKind::Node,
            node.id,
            node.keys_data,
            node.vals_data,
            &[],
        );
    }

    fn way(&mut self, way: &WireWay<'_>, _info: Option<&WireInfo>) {
        self.plain(
            ElementKind::Way,
            way.id,
            way.keys_data,
            way.vals_data,
            way.refs_data,
        );
    }

    fn relation(&mut self, relation: &WireRelation<'_>, _info: Option<&WireInfo>) {
        self.plain(
            ElementKind::Relation,
            relation.id,
            relation.keys_data,
            relation.vals_data,
            &[],
        );
    }

    fn dense(&mut self, dense: &WireDenseNodes<'_>, ids: usize, _info: Option<&WireDenseInfo<'_>>) {
        let tags = if self.kinds[0] {
            dense.keys_vals_data
        } else {
            &[]
        };
        if ids != 0 && (self.invert || !tags.is_empty()) {
            self.runs.push(Run::Dense {
                ids: Span::of(dense.id_data),
                tags: Span::of(tags),
            });
        }
    }
}

#[derive(Default)]
pub(super) struct ClassifyScratch {
    runs: Vec<Run>,
    masks: MaskScratch,
    pub(super) refs: Vec<i64>,
}

#[derive(Default)]
pub(super) struct Classified {
    pub(super) matched_nodes: Vec<i64>,
    pub(super) matched_ways: Vec<i64>,
    pub(super) matched_relations: Vec<i64>,
    pub(super) spilled_refs: Vec<i64>,
}

impl ClassifyScratch {
    pub(super) fn classify(
        &mut self,
        buffer: BlockBuffer<'_>,
        predicates: &TagPredicates<'_>,
        invert: bool,
    ) -> Result<Classified> {
        self.runs.clear();
        self.refs.clear();
        let kinds = [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
            .map(|kind| predicates.admits(kind));
        let (_, accepted) = buffer.project(|_, _| Runs {
            runs: &mut self.runs,
            kinds,
            invert,
        })?;
        let mut masks = self
            .masks
            .block(predicates, |sid| accepted.string(sid as usize));
        let mut result = Classified::default();
        let bytes = accepted.bytes();
        // Classification only inserts IDs into sets. Validation's plain-
        // before-dense order inside a group has no effect on those sets.
        for &run in &self.runs {
            match run {
                Run::Plain {
                    kind,
                    id,
                    keys,
                    vals,
                    refs,
                } => {
                    let tags = PackedUint32Iter::new(keys.in_blob(bytes)?)
                        .zip(PackedUint32Iter::new(vals.in_blob(bytes)?));
                    if masks.matches(kind, tags)? != invert {
                        match kind {
                            ElementKind::Node => result.matched_nodes.push(id),
                            ElementKind::Way => {
                                result.matched_ways.push(id);
                                self.refs
                                    .extend(WayRefIter::delta_decoded(refs.in_blob(bytes)?));
                            }
                            ElementKind::Relation => result.matched_relations.push(id),
                        }
                    }
                }
                Run::Dense { ids, tags } => {
                    classify_dense(
                        &mut masks,
                        ids.in_blob(bytes)?,
                        tags.in_blob(bytes)?,
                        invert,
                        &mut result.matched_nodes,
                    )?;
                }
            }
        }
        crate::retention::release_scratch(&mut self.runs);
        self.masks.release_outliers();
        Ok(result)
    }
}

fn classify_dense<'b>(
    masks: &mut BlockMasks<'_, '_, impl Fn(u32) -> Option<&'b str>>,
    ids: &[u8],
    tags: &[u8],
    invert: bool,
    matched: &mut Vec<i64>,
) -> Result<()> {
    let mut cursor = Cursor::new(tags);
    for id in WayRefIter::delta_decoded(ids) {
        let mut hit = false;
        // No tag column means every node is untagged. A nonempty column
        // delimits runs with a zero key and may end with a complete pair.
        while !cursor.is_empty() {
            let key = dense_sid(cursor.read_varint()?)?;
            if key == 0 {
                break;
            }
            let value = dense_sid(cursor.read_varint()?)?;
            if !hit {
                hit = masks.matches_tag(ElementKind::Node, key, value)?;
            }
        }
        if hit != invert {
            matched.push(id);
        }
    }
    assert!(
        cursor.is_empty(),
        "validated dense tag runs outlast their IDs"
    );
    Ok(())
}

fn dense_sid(value: u64) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| Error::internal("tags-filter: a validated dense tag SID exceeds uint32"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::block_builder::BlockBuilder;
    use crate::owned::WayCoords;
    use crate::read::wire_fixtures::{
        Group, block, dense, field_bytes, packed, plain_node, relation, way,
    };
    use crate::tag_expr::parse_expressions;
    use crate::{Element, PrimitiveBlock};

    fn key(element: &Element<'_>) -> (ElementKind, i64) {
        match element {
            Element::DenseNode(n) => (ElementKind::Node, n.id()),
            Element::Node(n) => (ElementKind::Node, n.id()),
            Element::Way(w) => (ElementKind::Way, w.id()),
            Element::Relation(r) => (ElementKind::Relation, r.id()),
        }
    }

    fn fixture() -> Vec<u8> {
        let mut r = relation(100, &[(0, -80, 1)]);
        field_bytes(&mut r, 2, &packed([7, 9]));
        field_bytes(&mut r, 3, &packed([8, 10]));
        block(
            &[
                "",
                "amenity",
                "school",
                "shop",
                "bakery",
                "amenity",
                "school",
                "shop",
                "bakery",
                "name",
                "East",
                "amenity:source",
            ],
            &[
                Group {
                    nodes: vec![
                        plain_node(10, 0, 0, &[(5, 6)], None),
                        plain_node(11, 0, 0, &[(1, 4), (3, 2)], None),
                        plain_node(12, 0, 0, &[(11, 10)], None),
                    ],
                    dense: Some(dense(
                        &[(1, 0, 0), (2, 0, 0), (3, 0, 0), (4, 0, 0), (5, 0, 0)],
                        &[1, 2, 0, 5, 8, 7, 6, 0, 0, 5, 0, 0, 7, 8],
                    )),
                    ways: vec![way(-80, &[1, 10, 1], (7, 8)), way(81, &[2, 3], (3, 6))],
                    relations: vec![r],
                },
                Group {
                    dense: Some(dense(&[(20, 0, 0), (21, 0, 0)], &[])),
                    ..Group::default()
                },
            ],
        )
    }

    #[test]
    fn filter_paths_match_a_string_oracle_on_mixed_groups() {
        let bytes = fixture();
        let block = PrimitiveBlock::from_vec(bytes.clone()).unwrap();
        let mut scratch = ClassifyScratch::default();
        let (mut st, mut groups) = (Vec::new(), Vec::new());
        let queries: &[&[&str]] = &[
            &[],
            &["amenity=school", "shop=bakery"],
            &["n/amenity=school", "w/shop=bakery", "r/name=E*"],
            &["n/amenity!=school", "w/shop!=*", "r/name=*as*"],
            &["n/amenity:*", "w/shop", "r/*=*"],
            &["n/amenity="],
        ];
        for query in queries {
            let expressions =
                parse_expressions(&query.iter().map(|&s| s.to_owned()).collect::<Vec<_>>())
                    .unwrap();
            let predicates = TagPredicates::new(&expressions);
            for invert in [false, true] {
                let mut expected = Vec::new();
                let mut refs = Vec::new();
                for element in block.elements() {
                    let tags: Vec<_> = match &element {
                        Element::DenseNode(n) => n.tags().collect(),
                        Element::Node(n) => n.tags().collect(),
                        Element::Way(w) => w.tags().collect(),
                        Element::Relation(r) => r.tags().collect(),
                    };
                    let (kind, id) = key(&element);
                    if super::super::element_matches(&expressions, &tags, kind) != invert {
                        expected.push((kind, id));
                        if let Element::Way(w) = element {
                            refs.extend(w.refs());
                        }
                    }
                }

                let buffer = BlockBuffer::new(bytes.clone(), None, &mut st, &mut groups, None);
                let selected = scratch.classify(buffer, &predicates, invert).unwrap();
                let mut classified = Vec::new();
                classified.extend(
                    selected
                        .matched_nodes
                        .into_iter()
                        .map(|id| (ElementKind::Node, id)),
                );
                classified.extend(
                    selected
                        .matched_ways
                        .into_iter()
                        .map(|id| (ElementKind::Way, id)),
                );
                classified.extend(
                    selected
                        .matched_relations
                        .into_iter()
                        .map(|id| (ElementKind::Relation, id)),
                );
                let mut sorted = expected.clone();
                sorted.sort_unstable();
                classified.sort_unstable();
                assert_eq!(classified, sorted, "{query:?}, invert={invert}");
                assert_eq!(scratch.refs, refs, "{query:?}, invert={invert}");

                let mut builder = BlockBuilder::new();
                let mut output = Vec::new();
                super::super::filter_block_parallel(
                    &block,
                    &predicates,
                    invert,
                    WayCoords::Drop,
                    &mut builder,
                    &mut output,
                )
                .unwrap();
                super::super::flush_local(&mut builder, &mut output).unwrap();
                let mut emitted = Vec::new();
                for output_block in output {
                    let output_block = PrimitiveBlock::from_vec(output_block.bytes).unwrap();
                    emitted.extend(output_block.elements().map(|element| key(&element)));
                }
                assert_eq!(emitted, expected, "{query:?}, invert={invert}");
            }
        }
    }
}
