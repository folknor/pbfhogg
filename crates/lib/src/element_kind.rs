//! The one "node, way or relation" type.

use std::str::FromStr;

/// An OSM element kind.
///
/// Declaration order is the OSM type-then-id order every sorted PBF uses:
/// node < way < relation. `Ord`, [`ordinal`](Self::ordinal) and the derived
/// comparison all follow it; the `order_and_spellings` unit test pins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementKind {
    /// A node.
    Node,
    /// A way.
    Way,
    /// A relation.
    Relation,
}

impl ElementKind {
    /// The lowercase OSM name: `"node"`, `"way"` or `"relation"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
        }
    }

    /// Position in the OSM type-then-id order: 0, 1 or 2.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Node => 0,
            Self::Way => 1,
            Self::Relation => 2,
        }
    }

    /// The one-letter prefix used in id specs: `'n'`, `'w'` or `'r'`.
    #[must_use]
    pub const fn type_char(self) -> char {
        match self {
            Self::Node => 'n',
            Self::Way => 'w',
            Self::Relation => 'r',
        }
    }

    /// The inverse of [`as_str`](Self::as_str): exactly `node`, `way` or
    /// `relation`, nothing else. Used where a file format fixes the spelling
    /// (OSC element names, relation member types).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "node" => Some(Self::Node),
            "way" => Some(Self::Way),
            "relation" => Some(Self::Relation),
            _ => None,
        }
    }

    /// The inverse of [`ordinal`](Self::ordinal).
    #[must_use]
    pub const fn from_ordinal(ordinal: u8) -> Option<Self> {
        match ordinal {
            0 => Some(Self::Node),
            1 => Some(Self::Way),
            2 => Some(Self::Relation),
            _ => None,
        }
    }
}

/// The element kinds a decoded block holds, as a set: what
/// [`PrimitiveBlock::element_kinds`](crate::PrimitiveBlock::element_kinds)
/// returns.
///
/// A kind is in the set when the block holds at least one element of it,
/// which is exactly when [`PrimitiveBlock::elements`](crate::PrimitiveBlock::elements)
/// yields one: a plain `Node` message or a dense group with at least one ID
/// (both [`ElementKind::Node`]), a `Way` or a `Relation` message. Empty
/// groups, an empty dense message, changeset groups and unknown fields add
/// nothing, so a block of only those has the empty set. Unlike
/// [`BlockType`](crate::BlockType), which labels each group by its first
/// field, this is a kind test: a block whose set is [`single`](Self::single)
/// holds elements of that one kind and no other.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Hash)]
pub struct ElementKindSet(u8);

impl ElementKindSet {
    const fn bit(kind: ElementKind) -> u8 {
        1 << kind.ordinal()
    }

    /// Whether `kind` is in the set.
    #[must_use]
    #[inline]
    pub fn has(self, kind: ElementKind) -> bool {
        self.0 & Self::bit(kind) != 0
    }

    /// Whether the set holds no kind: the block yields no element.
    #[must_use]
    #[inline]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many kinds the set holds: 0 to 3.
    #[must_use]
    #[inline]
    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The one kind the set holds, or `None` when it holds none or several.
    #[must_use]
    #[inline]
    pub fn single(self) -> Option<ElementKind> {
        [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
            .into_iter()
            .find(|&kind| self.0 == Self::bit(kind))
    }

    /// The kinds in the set, in node, way, relation order.
    pub fn iter(self) -> impl Iterator<Item = ElementKind> {
        [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
            .into_iter()
            .filter(move |&kind| self.has(kind))
    }

    /// Add `kind` to the set.
    #[inline]
    pub(crate) fn add(&mut self, kind: ElementKind) {
        self.0 |= Self::bit(kind);
    }

    /// The set holding both sets' kinds.
    #[must_use]
    pub(crate) fn union(self, other: ElementKindSet) -> ElementKindSet {
        ElementKindSet(self.0 | other.0)
    }

    /// A kind in the set other than `claimed`, if any: the witness that a
    /// claim of `claimed` is false. Chosen by the fixed node, way, relation
    /// priority, so the same block always yields the same witness. This is
    /// the test against a blob's indexdata claim, deliberately weaker than
    /// `check --index`: an empty block under any claim holds no foreign kind.
    #[must_use]
    #[inline]
    pub(crate) fn foreign_to(self, claimed: ElementKind) -> Option<ElementKind> {
        if self.0 & !Self::bit(claimed) == 0 {
            return None;
        }
        [ElementKind::Node, ElementKind::Way, ElementKind::Relation]
            .into_iter()
            .find(|&kind| kind != claimed && self.has(kind))
    }

    /// Refuse the set when `claim` names a kind and the set holds another:
    /// `IndexdataMismatch` with the `KindContradiction`, unlocated (the
    /// caller locates it at the blob). A blob with no claim is never refused.
    /// One compare when the set agrees.
    #[inline]
    pub(crate) fn refuse_under(self, claim: Option<ElementKind>) -> crate::error::Result<()> {
        if let Some(claimed) = claim
            && let Some(found) = self.foreign_to(claimed)
        {
            return Err(crate::error::new_kind_contradiction(claimed, found));
        }
        Ok(())
    }
}

impl std::fmt::Display for ElementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ElementKind {
    type Err = crate::Error;

    /// Accepts the full names `node`, `way`, `relation` and the one-letter
    /// forms `n`, `w`, `r`. Nothing else, no case folding. Anything else is
    /// an `InvalidArgument` error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "n" => Ok(Self::Node),
            "w" => Ok(Self::Way),
            "r" => Ok(Self::Relation),
            other => Self::from_name(other).ok_or_else(|| {
                crate::Error::invalid_argument(format!(
                    "invalid element kind '{other}': expected node, way or relation"
                ))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ElementKind;

    #[test]
    fn order_and_spellings() {
        let all = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];
        assert!(ElementKind::Node < ElementKind::Way);
        assert!(ElementKind::Way < ElementKind::Relation);
        assert_eq!(size_of::<ElementKind>(), 1);
        for (i, kind) in all.into_iter().enumerate() {
            assert_eq!(usize::from(kind.ordinal()), i);
            assert_eq!(ElementKind::from_ordinal(kind.ordinal()), Some(kind));
        }
        assert_eq!(ElementKind::from_ordinal(3), None);
        assert_eq!(ElementKind::Node.as_str(), "node");
        assert_eq!(ElementKind::Way.as_str(), "way");
        assert_eq!(ElementKind::Relation.as_str(), "relation");
        assert_eq!(ElementKind::Node.type_char(), 'n');
        assert_eq!(ElementKind::Way.type_char(), 'w');
        assert_eq!(ElementKind::Relation.type_char(), 'r');
        for kind in all {
            assert_eq!(kind.as_str().parse::<ElementKind>().ok(), Some(kind));
            assert_eq!(
                kind.type_char().to_string().parse::<ElementKind>().ok(),
                Some(kind)
            );
        }
        crate::test_support::assert_err_kind!(
            "Node".parse::<ElementKind>(),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("invalid element kind 'Node'"),
        );
        crate::test_support::assert_err_kind!(
            "".parse::<ElementKind>(),
            crate::ErrorKind::InvalidArgument { msg, .. } if msg.contains("invalid element kind ''"),
        );
    }

    #[test]
    fn a_kind_set_names_a_foreign_kind_by_fixed_priority() {
        use super::ElementKindSet;
        let mut set = ElementKindSet::default();
        for claim in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
            assert_eq!(set.foreign_to(claim), None, "empty set under {claim}");
        }
        set.add(ElementKind::Way);
        assert_eq!(set.foreign_to(ElementKind::Way), None);
        assert_eq!(set.foreign_to(ElementKind::Node), Some(ElementKind::Way));
        set.add(ElementKind::Relation);
        set.add(ElementKind::Node);
        assert_eq!(
            set.foreign_to(ElementKind::Relation),
            Some(ElementKind::Node)
        );
        assert_eq!(set.foreign_to(ElementKind::Node), Some(ElementKind::Way));
        assert!(set.has(ElementKind::Node));
    }

    #[test]
    fn a_kind_set_is_single_only_with_exactly_one_kind() {
        use super::ElementKindSet;
        let all = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];
        let empty = ElementKindSet::default();
        assert!(empty.is_empty());
        assert_eq!((empty.len(), empty.single()), (0, None));
        assert_eq!(empty.iter().count(), 0);
        for kind in all {
            let mut one = ElementKindSet::default();
            one.add(kind);
            assert!(!one.is_empty());
            assert_eq!((one.len(), one.single()), (1, Some(kind)));
            assert_eq!(one.iter().collect::<Vec<_>>(), [kind]);
        }
        let mut several = ElementKindSet::default();
        several.add(ElementKind::Relation);
        several.add(ElementKind::Node);
        assert_eq!((several.len(), several.single()), (2, None));
        assert_eq!(
            several.iter().collect::<Vec<_>>(),
            [ElementKind::Node, ElementKind::Relation]
        );
        several.add(ElementKind::Way);
        assert_eq!(several.iter().collect::<Vec<_>>(), all);
        assert_eq!(several.single(), None);
    }
}
