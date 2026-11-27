//! Block-local tag predicate evaluation over string table indices.

use rustc_hash::FxHashMap;

use crate::ElementKind;
use crate::error::{Error, Result};
use crate::tag_expr::{Expression, StringMatcher};
use crate::tunables::MAX_RETAINED_SCRATCH_CAPACITY;

type Bits = u64;

/// A machine word of expression identities. Larger expression sets use
/// successive words; a key and value can only meet at the same bit.
#[derive(Default)]
struct Word {
    kinds: [Bits; 3],
    key_only: Bits,
    always_keys: Bits,
    always_values: Bits,
    negated_values: Bits,
    keys: Vec<MatcherBits>,
    values: Vec<MatcherBits>,
}

struct MatcherBits {
    matcher: usize,
    bits: Bits,
}

struct Matcher<'a> {
    pattern: &'a StringMatcher,
    shared: bool,
}

/// Compiled once for the run, shared by the workers.
pub(in crate::commands) struct TagPredicates<'a> {
    words: Vec<Word>,
    matchers: Vec<Matcher<'a>>,
}

impl<'a> TagPredicates<'a> {
    pub(in crate::commands) fn new(expressions: &'a [Expression]) -> Self {
        let mut words = Vec::new();
        let mut ids = FxHashMap::default();
        let mut matchers = Vec::<Matcher<'a>>::new();
        let mut intern = |pattern: &'a StringMatcher| {
            let next = matchers.len();
            let id = *ids.entry(matcher_identity(pattern)).or_insert(next);
            if id == next {
                matchers.push(Matcher {
                    pattern,
                    shared: false,
                });
            } else {
                matchers[id].shared = true;
            }
            id
        };
        for chunk in expressions.chunks(Bits::BITS as usize) {
            let mut word = Word::default();
            for (index, expr) in chunk.iter().enumerate() {
                let bit = 1 << index;
                for kind in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
                    if expr.type_filter.admits(kind) {
                        word.kinds[kind.ordinal() as usize] |= bit;
                    }
                }
                if matches!(expr.matcher.key, StringMatcher::Always) {
                    word.always_keys |= bit;
                } else {
                    add_matcher(&mut word.keys, intern(&expr.matcher.key), bit);
                }
                if let Some(value) = &expr.matcher.value {
                    if matches!(value, StringMatcher::Always) {
                        word.always_values |= bit;
                    } else {
                        add_matcher(&mut word.values, intern(value), bit);
                    }
                    if expr.matcher.invert {
                        word.negated_values |= bit;
                    }
                } else {
                    word.key_only |= bit;
                }
            }
            words.push(word);
        }
        Self { words, matchers }
    }

    /// With no expression, tags-count counts every key occurrence.
    pub(in crate::commands) fn all_keys() -> Self {
        Self {
            words: vec![Word {
                kinds: [1; 3],
                key_only: 1,
                always_keys: 1,
                ..Word::default()
            }],
            matchers: Vec::new(),
        }
    }

    pub(in crate::commands) fn admits(&self, kind: ElementKind) -> bool {
        self.words
            .iter()
            .any(|word| word.kinds[kind.ordinal() as usize] != 0)
    }
}

fn matcher_identity(matcher: &StringMatcher) -> (std::mem::Discriminant<StringMatcher>, &[String]) {
    let strings = match matcher {
        StringMatcher::Always => &[],
        StringMatcher::Equal(s) | StringMatcher::Prefix(s) | StringMatcher::Substring(s) => {
            std::slice::from_ref(s)
        }
        StringMatcher::List(strings) => strings.as_slice(),
    };
    (std::mem::discriminant(matcher), strings)
}

fn add_matcher(groups: &mut Vec<MatcherBits>, matcher: usize, bit: Bits) {
    if let Some(group) = groups.iter_mut().find(|g| g.matcher == matcher) {
        group.bits |= bit;
    } else {
        groups.push(MatcherBits { matcher, bits: bit });
    }
}

#[derive(Default)]
struct Tested {
    known: Bits,
    matched: Bits,
}

#[derive(Default)]
struct WordCache {
    keys: FxHashMap<u32, Tested>,
    values: FxHashMap<u32, Tested>,
}

#[derive(Default)]
pub(in crate::commands) struct MaskScratch {
    words: Vec<WordCache>,
    // Only matchers with multiple uses need this second level. A single-use
    // matcher is fully covered by its word's SID masks.
    shared: FxHashMap<(usize, u32), bool>,
}

impl MaskScratch {
    pub(in crate::commands) fn block<'s, 'p, 'b, F: Fn(u32) -> Option<&'b str>>(
        &'s mut self,
        predicates: &'p TagPredicates<'p>,
        string: F,
    ) -> BlockMasks<'s, 'p, F> {
        self.words
            .resize_with(predicates.words.len(), WordCache::default);
        for word in &mut self.words {
            word.keys.clear();
            word.values.clear();
        }
        self.shared.clear();
        BlockMasks {
            predicates,
            words: &mut self.words,
            shared: &mut self.shared,
            string,
        }
    }

    pub(in crate::commands) fn release_outliers(&mut self) {
        for word in &mut self.words {
            release_outlier(&mut word.keys);
            release_outlier(&mut word.values);
        }
        release_outlier(&mut self.shared);
    }
}

fn release_outlier<K, V>(cache: &mut FxHashMap<K, V>) {
    if cache.capacity().saturating_mul(size_of::<(K, V)>()) > MAX_RETAINED_SCRATCH_CAPACITY {
        *cache = FxHashMap::default();
    }
}

pub(in crate::commands) struct BlockMasks<'s, 'p, F> {
    predicates: &'p TagPredicates<'p>,
    words: &'s mut [WordCache],
    shared: &'s mut FxHashMap<(usize, u32), bool>,
    string: F,
}

#[derive(Default)]
pub(in crate::commands) struct TagHits {
    pub(in crate::commands) key: bool,
    pub(in crate::commands) pair: bool,
}

impl<'b, F: Fn(u32) -> Option<&'b str>> BlockMasks<'_, '_, F> {
    pub(super) fn matches(
        &mut self,
        kind: ElementKind,
        tags: impl IntoIterator<Item = (u32, u32)>,
    ) -> Result<bool> {
        for (key, value) in tags {
            if self.matches_tag(kind, key, value)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn matches_tag(&mut self, kind: ElementKind, key: u32, value: u32) -> Result<bool> {
        let hits = self.select(kind, key, value, false)?;
        Ok(hits.key || hits.pair)
    }

    pub(in crate::commands) fn rows(
        &mut self,
        kind: ElementKind,
        key: u32,
        value: u32,
    ) -> Result<TagHits> {
        self.select(kind, key, value, true)
    }

    fn select(
        &mut self,
        kind: ElementKind,
        key: u32,
        value: u32,
        count_rows: bool,
    ) -> Result<TagHits> {
        let mut hits = TagHits::default();
        let mut lookup = Lookup {
            matchers: &self.predicates.matchers,
            shared: self.shared,
            string: &self.string,
        };
        for (word, cache) in self.predicates.words.iter().zip(&mut *self.words) {
            let mut active = word.kinds[kind.ordinal() as usize];
            if hits.key {
                active &= !word.key_only;
            }
            if hits.pair {
                active &= word.key_only;
            }
            let keys = match_sid(
                &mut lookup,
                &mut cache.keys,
                &word.keys,
                key,
                active,
                word.always_keys,
            )?;
            hits.key |= keys & word.key_only != 0;
            if hits.key && !count_rows {
                break;
            }
            let candidates = keys & !word.key_only;
            if !hits.pair && candidates != 0 {
                let values = match_sid(
                    &mut lookup,
                    &mut cache.values,
                    &word.values,
                    value,
                    candidates,
                    word.always_values,
                )?;
                hits.pair = candidates & (values ^ word.negated_values) != 0;
            }
            if hits.pair && (!count_rows || hits.key) {
                break;
            }
        }
        Ok(hits)
    }
}

struct Lookup<'a, 'p, F> {
    matchers: &'a [Matcher<'p>],
    shared: &'a mut FxHashMap<(usize, u32), bool>,
    string: &'a F,
}

/// Unconditional bits need no lookup. For other bits, evaluate only the
/// groups whose result is still needed. In particular a value is tested only
/// against expressions whose key matched and whose kind applies.
fn match_sid<'b>(
    lookup: &mut Lookup<'_, '_, impl Fn(u32) -> Option<&'b str>>,
    cache: &mut FxHashMap<u32, Tested>,
    groups: &[MatcherBits],
    sid: u32,
    requested: Bits,
    always: Bits,
) -> Result<Bits> {
    let needed = requested & !always;
    if needed == 0 {
        return Ok(requested);
    }
    let tested = cache.entry(sid).or_default();
    let missing = needed & !tested.known;
    if missing != 0 {
        let mut text = None;
        for group in groups.iter().filter(|group| group.bits & missing != 0) {
            let matcher = &lookup.matchers[group.matcher];
            let shared_key = (group.matcher, sid);
            let cached = if matcher.shared {
                lookup.shared.get(&shared_key).copied()
            } else {
                None
            };
            let matched = if let Some(result) = cached {
                result
            } else {
                let s = match text {
                    Some(s) => s,
                    None => {
                        let s = (lookup.string)(sid).ok_or_else(|| {
                            Error::internal(
                                "tag predicate: a validated SID lies past its string table",
                            )
                        })?;
                        text = Some(s);
                        s
                    }
                };
                let result = matcher.pattern.matches(s);
                if matcher.shared {
                    lookup.shared.insert(shared_key, result);
                }
                result
            };
            tested.known |= group.bits;
            if matched {
                tested.matched |= group.bits;
            }
        }
    }
    Ok(requested & (tested.matched | always))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::tag_expr::{parse_expression, tag_matches};

    #[test]
    fn sparse_masks_share_matchers_across_words_and_skip_unneeded_values() {
        let mut expressions = vec![parse_expression("n/k=v").unwrap(); Bits::BITS as usize];
        expressions.push(parse_expression("w/k=v").unwrap());
        expressions.push(parse_expression("w/j!=v").unwrap());
        expressions.push(parse_expression("r/k").unwrap());
        let predicates = TagPredicates::new(&expressions);
        let strings = ["", "k", "j", "v", "v", "unused"];
        let reads = [const { Cell::new(0_u32) }; 6];
        let mut scratch = MaskScratch::default();
        let mut masks = scratch.block(&predicates, |sid| {
            let read = &reads[sid as usize];
            read.set(read.get() + 1);
            strings.get(sid as usize).copied()
        });

        // The relation's key-only expression reads the key and leaves the value alone.
        assert!(masks.matches_tag(ElementKind::Relation, 1, 5).unwrap());
        assert_eq!(reads[1].get(), 1);
        assert_eq!(reads[5].get(), 0);
        // The key rejects the pair before the value SID is inspected.
        assert!(!masks.matches_tag(ElementKind::Node, 2, 5).unwrap());
        assert_eq!(reads[5].get(), 0);
        // The node word reuses the relation's `k` result. The way word also
        // needs the distinct `j` matcher on that same key SID, once.
        for (kind, key_reads) in [(ElementKind::Node, 1), (ElementKind::Way, 2)] {
            for value in [3, 4, 3] {
                let expected = expressions.iter().any(|expr| {
                    expr.type_filter.admits(kind)
                        && tag_matches(&expr.matcher, strings[1], strings[value as usize])
                });
                assert_eq!(masks.matches_tag(kind, 1, value).unwrap(), expected);
            }
            assert_eq!(reads[1].get(), key_reads, "{kind:?}");
        }
        // Both words share the `v` matcher, including its use under negation.
        assert_eq!(reads[3].get(), 1);
        assert_eq!(reads[4].get(), 1);
        assert!(!masks.matches_tag(ElementKind::Way, 2, 3).unwrap());
        assert!(masks.matches_tag(ElementKind::Way, 2, 0).unwrap());
        assert_eq!(reads[3].get(), 1);
        assert_eq!(reads[5].get(), 0);
    }

    #[test]
    fn a_cached_value_sid_can_gain_bits_for_another_key() {
        let expressions = [
            parse_expression("amenity=school").unwrap(),
            parse_expression("shop=bakery").unwrap(),
        ];
        let predicates = TagPredicates::new(&expressions);
        let strings = ["", "amenity", "shop", "school", "bakery"];
        let mut scratch = MaskScratch::default();
        let mut masks = scratch.block(&predicates, |sid| strings.get(sid as usize).copied());

        assert!(!masks.matches_tag(ElementKind::Node, 1, 4).unwrap());
        assert!(masks.matches_tag(ElementKind::Node, 2, 4).unwrap());
        assert!(masks.matches_tag(ElementKind::Node, 1, 3).unwrap());
        assert!(!masks.matches_tag(ElementKind::Node, 2, 3).unwrap());
    }
}
